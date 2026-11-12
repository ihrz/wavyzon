//! The [`Plugin`] trait and the [`Registry`] plugins fill at startup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use serde::Serialize;

use crate::{Event, FilterFactory, LyricsProvider, Source};

/// Version of this API. The node refuses plugins built for another major.
pub const API_VERSION: u32 = 1;

/// Static description of a plugin, shown in `GET /v1/info` and `GET /v1/plugins`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// Lowercase, URL-safe name (`"hello"`). Also the route prefix
    /// `/v1/plugins/<name>/…` and the env prefix `WAVYZON_PLUGIN_<NAME>_`.
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    /// Must equal [`API_VERSION`].
    pub api_version: u32,
}

/// Receives every player event of every session, including Wavyzon-specific
/// events a Lavalink v4 client did not opt into. Runs on a dedicated task;
/// slow listeners only delay themselves.
#[async_trait]
pub trait EventListener: Send + Sync + 'static {
    async fn on_event(&self, session_id: &str, event: &Event);
}

/// What a plugin is given at startup.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// `WAVYZON_PLUGIN_<NAME>_*` variables, keyed by the suffix in lowercase
    /// (`WAVYZON_PLUGIN_HELLO_GREETING=hi` → `"greeting" => "hi"`).
    pub settings: HashMap<String, String>,
    /// A private, writable directory (`$WAVYZON_DATA_DIR/plugins/<name>`). The
    /// node creates it before calling [`Plugin::register`].
    pub data_dir: PathBuf,
    /// The node's version string.
    pub node_version: &'static str,
}

impl PluginContext {
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(&key.to_ascii_lowercase()).map(String::as_str)
    }

    /// Build the context for `name` from the process environment.
    pub fn from_env(name: &str, data_root: &std::path::Path, node_version: &'static str) -> Self {
        let prefix = format!("WAVYZON_PLUGIN_{}_", name.to_ascii_uppercase().replace('-', "_"));
        let settings = std::env::vars()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|s| (s.to_ascii_lowercase(), v)))
            .collect();
        Self {
            settings,
            data_dir: data_root.join("plugins").join(name),
            node_version,
        }
    }
}

/// Everything plugins contribute.
#[derive(Default)]
pub struct Registry {
    pub(crate) sources: Vec<Arc<dyn Source>>,
    pub(crate) filters: Vec<Arc<dyn FilterFactory>>,
    pub(crate) listeners: Vec<Arc<dyn EventListener>>,
    pub(crate) routes: Vec<(String, Router)>,
    pub(crate) lyrics: Vec<Arc<dyn LyricsProvider>>,
    current: Option<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source. It is consulted before the built-in ones for links it
    /// matches, and its search prefix becomes usable in `loadtracks`.
    pub fn source(&mut self, source: Arc<dyn Source>) -> &mut Self {
        self.sources.push(source);
        self
    }

    /// Add a filter usable under `filters.pluginFilters.<name>`.
    pub fn filter(&mut self, factory: Arc<dyn FilterFactory>) -> &mut Self {
        self.filters.push(factory);
        self
    }

    /// Receive every player event.
    pub fn listener(&mut self, listener: Arc<dyn EventListener>) -> &mut Self {
        self.listeners.push(listener);
        self
    }

    /// Add a lyrics provider, asked before the node's lyrics.ovh client
    /// (providers of several plugins are asked in plugin order).
    pub fn lyrics(&mut self, provider: Arc<dyn LyricsProvider>) -> &mut Self {
        self.lyrics.push(provider);
        self
    }

    /// Mount `router` at `/v1/plugins/<plugin name>`. Requests are
    /// authenticated by the node before they reach it.
    pub fn routes(&mut self, router: Router) -> &mut Self {
        let name = self.current.clone().unwrap_or_else(|| "unnamed".into());
        self.routes.push((name, router));
        self
    }

    #[doc(hidden)]
    pub fn begin(&mut self, plugin: &str) {
        self.current = Some(plugin.to_string());
    }

    pub fn sources(&self) -> &[Arc<dyn Source>] {
        &self.sources
    }
    pub fn filters(&self) -> &[Arc<dyn FilterFactory>] {
        &self.filters
    }
    pub fn listeners(&self) -> &[Arc<dyn EventListener>] {
        &self.listeners
    }
    pub fn lyrics_providers(&self) -> &[Arc<dyn LyricsProvider>] {
        &self.lyrics
    }
    /// Take the routers out (the server mounts them once).
    pub fn take_routes(&mut self) -> Vec<(String, Router)> {
        std::mem::take(&mut self.routes)
    }
}

/// A Wavyzon plugin.
///
/// ```no_run
/// use std::sync::Arc;
/// use wavyzon_plugin::*;
///
/// struct Hello;
///
/// #[async_trait]
/// impl Plugin for Hello {
///     fn manifest(&self) -> PluginManifest {
///         PluginManifest { name: "hello", version: "1.0.0", description: "Says hi", api_version: API_VERSION }
///     }
///     async fn register(&self, _ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
///         reg.routes(axum::Router::new().route("/", axum::routing::get(|| async { "hi" })));
///         Ok(())
///     }
/// }
/// ```
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn manifest(&self) -> PluginManifest;

    /// Register sources, filters, listeners, lyrics providers and routes. Called once, in
    /// plugin order, before the server starts listening.
    async fn register(&self, ctx: &PluginContext, registry: &mut Registry) -> Result<(), PluginError>;

    /// Called once on graceful shutdown.
    async fn shutdown(&self) {}
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin {plugin} targets API v{found}, this node speaks v{expected}")]
    ApiVersion { plugin: String, found: u32, expected: u32 },
    #[error("plugin configuration: {0}")]
    Config(String),
    #[error("{0}")]
    Other(String),
}
