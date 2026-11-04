//! Wiring: build every component from a [`Config`], serve, shut down.

use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::{Router, ServiceExt};
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
        let stopped = stop_http.clone();
        let app = ServiceExt::<axum::extract::Request>::into_make_service(self.service());
        let mut server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { stopped.cancelled().await })
                .await
        });
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
