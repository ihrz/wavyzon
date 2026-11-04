//! The `wavyzon` binary.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use wavyzon::config::{Config, LogFormat};
use wavyzon::{Node, api, plugins};

fn init_logs(cfg: &Config) {
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) if !v.trim().is_empty() => EnvFilter::new(v),
        _ => EnvFilter::new(match cfg.log_level.as_str() {
            "debug" | "trace" | "warn" | "error" => cfg.log_level.as_str(),
            _ => "info",
        }),
    };
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match cfg.log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.init(),
    }
}

/// Whether this build can do voice: DAVE and a working Opus encoder.
fn native_ready() -> Result<(), String> {
    if !wavyzon_voice::dave_available() {
        return Err(wavyzon_voice::Error::NoDave.to_string());
    }
    wavyzon_audio::codec::new_encoder(wavyzon_audio::codec::DEFAULT_BITRATE)
        .map(drop)
        .map_err(|e| e.to_string())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!(signal = "SIGINT", "signal received"),
        _ = term => tracing::info!(signal = "SIGTERM", "signal received"),
    }
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    if let Err(e) = native_ready() {
        if !cfg.allow_no_voice {
            anyhow::bail!("{e} (set WAVYZON_ALLOW_NO_VOICE=true to start anyway, without voice)");
        }
        tracing::error!(error = %e, "voice is disabled in this build; only search and analysis will work");
    }
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| anyhow::anyhow!("WAVYZON_DATA_DIR {}: {e}", cfg.data_dir.display()))?;
    let address = cfg.address.clone();
    let node = Node::build(cfg, plugins::compiled_in()).await?;
    node.restore().await;
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| anyhow::anyhow!("listen on {address}: {e}"))?;
    tracing::info!(
        address = %listener.local_addr().map(|a| a.to_string()).unwrap_or(address),
        version = api::VERSION,
        sources = ?node.sources.names(),
        plugins = ?node.api.plugins.iter().map(|p| p.name).collect::<Vec<_>>(),
        dave = wavyzon_voice::dave_available(),
        "wavyzon listening"
    );
    node.serve(listener, shutdown_signal()).await
}

fn main() -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wavyzon: {e}");
            return ExitCode::FAILURE;
        }
    };
    init_logs(&cfg);
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wavyzon: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(run(cfg));
    // Don't wait for blocking work (an offline analysis decoding a long track).
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "wavyzon stopped");
            ExitCode::FAILURE
        }
    }
}
