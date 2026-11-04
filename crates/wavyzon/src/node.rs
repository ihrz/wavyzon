//! Wiring: build every component from a [`Config`], serve, shut down.

use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::normalize_path::NormalizePath;
use wavyzon_analysis::{Offline, Registry as Fingerprints};
use wavyzon_player::filter_registry;
use wavyzon_plugin::{Failure, Plugin, Stream, Track};
use wavyzon_sources::Sources;

use crate::api::{self, Api, VERSION};
use crate::config::Config;
use crate::metrics::NodeMetrics;
use crate::peercache::Pool;
use crate::plugins::{self, EventBus};
use crate::session::{Deps, Manager, unix_ms};

/// How long in-flight HTTP requests get after the shutdown began.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// A fully wired node, not yet listening.
pub struct Node {
    pub config: Config,
    pub api: Arc<Api>,
    pub sessions: Arc<Manager>,
    pub sources: Sources,
    pub plugins: Vec<Arc<dyn Plugin>>,
    router: Router,
    peers: Option<Arc<Pool>>,
    stop_peers: CancellationToken,
}

impl Node {
    /// Load the plugins named by the config (out of `available`), build the
    /// sources, analysis, sessions and the router. Must run inside Tokio.
    pub async fn build(config: Config, available: Vec<Arc<dyn Plugin>>) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Handle::current();
        let mut loaded = plugins::load(available, &config.plugins, &config.data_dir, VERSION).await?;
        let registry = &mut loaded.registry;

        let sources = Sources::builder(&config.sources)
            .context("building sources")?
            .plugins(registry.sources().iter().cloned())
            .build();

        let fingerprints = Arc::new(Fingerprints::new());
        let opener_sources = sources.clone();
        let opener: wavyzon_analysis::Opener =
            Arc::new(move |t: Track| -> BoxFuture<'static, Result<Stream, Failure>> {
                let s = opener_sources.clone();
                async move {
                    let playable = s.playable(&t).await?;
                    s.open(&playable, 0).await
                }
                .boxed()
            });
        let offline = Arc::new(Offline::new(
            fingerprints.clone(),
            opener,
            config.analysis_workers,
            config.analysis_queue,
        ));

        let filters = Arc::new(filter_registry(registry.filters()));
        let mut plugin_filters: Vec<String> = filters.keys().cloned().collect();
        plugin_filters.sort();
        let bus = EventBus::new(registry.listeners(), &rt);
        let metrics = Arc::new(NodeMetrics::default());
        let peers = if config.peer_cache_enabled() {
            Pool::new(
                &config.cache_peers,
                &config.cache_secret,
                sources.audio_cache(),
                metrics.clone(),
            )
        } else {
            None
        };

        let sessions = Manager::new(Deps {
            sources: sources.clone(),
            fingerprints: fingerprints.clone(),
            offline: offline.clone(),
            filters,
            buffer_frames: config.buffer_frames(),
            playout_delay: config.playout_delay,
            pause_fade: config.pause_fade,
            update_interval: config.update_interval,
            normalize_lufs: config.normalize.then_some(config.normalize_lufs as f64),
            metrics: metrics.clone(),
            bus,
            peers: peers.clone(),
            runtime: rt,
        });
        let api = Arc::new(Api {
            password: config.password.clone(),
            cache_secret: config.cache_secret.clone(),
            sessions: sessions.clone(),
            sources: sources.clone(),
            fingerprints,
            offline,
            resume_timeout: config.resume_timeout,
            bitrate: config.bitrate,
            started: Instant::now(),
            started_unix_ms: unix_ms(),
            plugins: loaded.manifests.clone(),
            plugin_filters,
            metrics,
        });
        let weak: Weak<Api> = Arc::downgrade(&api);
        sessions.set_stats(Arc::new(move || {
            weak.upgrade().map(|a| a.stats_v4()).unwrap_or_default()
        }));
        let router = api::router(api.clone(), registry.take_routes());
        Ok(Self {
            config,
            api,
            sessions,
            sources,
            plugins: loaded.plugins,
            router,
            peers,
            stop_peers: CancellationToken::new(),
        })
    }

    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// The router with trailing slashes trimmed before routing, so
    /// `/v1/plugins/example/` reaches the plugin's `/` route.
    pub fn service(&self) -> NormalizePath<Router> {
        NormalizePath::trim_trailing_slash(self.router.clone())
    }

    /// Restore sessions saved by the last shutdown (`WAVYZON_STATE_FILE`).
    pub async fn restore(&self) {
        let Some(path) = &self.config.state_file else { return };
        match self.sessions.restore(path).await {
            Err(e) => tracing::error!(file = %path.display(), error = %e, "could not restore saved sessions"),
            Ok(rep) if rep.sessions + rep.skipped > 0 => tracing::info!(
                sessions = rep.sessions,
                players = rep.players,
                expired = rep.skipped,
                age_ms = rep.age.as_millis() as u64,
                "restored sessions from the last shutdown"
            ),
            Ok(_) => {}
        }
    }

    /// Serve on `listener` until `signal` resolves, then shut down: save the
    /// state file, close every session and voice connection, stop plugins,
    /// and give HTTP requests [`SHUTDOWN_GRACE`] to finish.
    pub async fn serve(
        self,
        listener: TcpListener,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        if let Some(p) = &self.peers {
            tokio::spawn(p.clone().run(self.stop_peers.clone()));
        }
        let stop_http = CancellationToken::new();
        let mut server = tokio::spawn(serve_http(
            listener,
            self.service(),
            stop_http.clone(),
            READ_HEADER_TIMEOUT,
        ));
        tokio::select! {
            r = &mut server => {
                self.shutdown().await;
                return r.context("server task")?.context("server");
            }
            _ = signal => tracing::info!("shutting down"),
        }
        self.shutdown().await;
        stop_http.cancel();
        match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
            Ok(r) => r.context("server task")?.context("server")?,
            Err(_) => {
                tracing::warn!("requests still running after the shutdown grace period, dropping them");
                server.abort();
            }
        }
        Ok(())
    }

    /// Save state, destroy sessions (closing voice) and stop plugins.
    pub async fn shutdown(&self) {
        self.stop_peers.cancel();
        if let Some(path) = &self.config.state_file {
            match self.sessions.save(path) {
                Ok(n) => tracing::info!(file = %path.display(), sessions = n, "saved sessions for resume"),
                Err(e) => tracing::error!(file = %path.display(), error = %e, "could not save sessions for resume"),
            }
        }
        self.sessions.shutdown().await;
        for p in &self.plugins {
            p.shutdown().await;
        }
    }
}

/// A client must send a request's headers within this long (Cadence's
/// `ReadHeaderTimeout`), so slow-header connections can't pile up.
pub const READ_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Accept HTTP/1 connections until `stop` fires, then let open requests
/// finish (upgraded WebSockets live on in their own tasks). This is
/// `axum::serve` plus a header-read timeout, which axum doesn't expose.
async fn serve_http(
    listener: TcpListener,
    service: NormalizePath<Router>,
    stop: CancellationToken,
    header_timeout: Duration,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;

    let conns = tokio_util::task::TaskTracker::new();
    loop {
        let tcp = tokio::select! {
            _ = stop.cancelled() => break,
            r = listener.accept() => match r {
                Ok((tcp, _)) => tcp,
                Err(e) => {
                    // Per-connection errors (reset before accept…) are
                    // harmless; anything else (EMFILE…) gets a short pause.
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                    ) {
                        tracing::error!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
        };
        let _ = tcp.set_nodelay(true);
        let svc = TowerToHyperService::new(service.clone());
        let stop = stop.clone();
        conns.spawn(async move {
            let mut b = hyper::server::conn::http1::Builder::new();
            b.timer(TokioTimer::new()).header_read_timeout(header_timeout);
            let mut conn = std::pin::pin!(b.serve_connection(TokioIo::new(tcp), svc).with_upgrades());
            tokio::select! {
                r = conn.as_mut() => {
                    if let Err(e) = r {
                        tracing::debug!(error = %e, "http connection ended with an error");
                    }
                    return;
                }
                _ = stop.cancelled() => conn.as_mut().graceful_shutdown(),
            }
            let _ = conn.await;
        });
    }
    conns.close();
    conns.wait().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn start(
        header_timeout: Duration,
    ) -> (String, CancellationToken, tokio::task::JoinHandle<std::io::Result<()>>) {
        let app = Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve_http(
            listener,
            NormalizePath::trim_trailing_slash(app),
            stop.clone(),
            header_timeout,
        ));
        (addr, stop, task)
    }

    #[tokio::test]
    async fn slow_headers_are_cut_off() {
        let (addr, stop, task) = start(Duration::from_millis(200)).await;
        let mut slow = tokio::net::TcpStream::connect(&addr).await.unwrap();
        slow.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        let mut buf = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(3), slow.read_to_end(&mut buf)).await;
        assert!(read.is_ok(), "connection with unfinished headers was kept open");
        // Normal requests (and trailing-slash normalisation) still work.
        let body = reqwest::get(format!("http://{addr}/ok/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_closes_idle_keep_alive_connections() {
        let (addr, stop, task) = start(Duration::from_secs(10)).await;
        let mut c = tokio::net::TcpStream::connect(&addr).await.unwrap();
        c.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 256];
        let n = c.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("an idle keep-alive connection held the shutdown")
            .unwrap()
            .unwrap();
    }
}
