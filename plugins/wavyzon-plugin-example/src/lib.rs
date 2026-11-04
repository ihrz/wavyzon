//! # wavyzon-plugin-example
//!
//! A complete, working [Wavyzon](https://gitlab.com/ihrz/wavyzon) plugin that
//! uses every extension point of [`wavyzon_plugin`]:
//!
//! | Extension | Here | Try it |
//! | --- | --- | --- |
//! | [`Source`] + [`Playable`] | [`ToneSource`]: a generated sine tone, streamed as [`Format::Pcm48`] | `GET /v1/loadtracks?identifier=tone:440` |
//! | [`FilterFactory`] | [`BitcrushFactory`]: bit-depth and sample-rate reduction | `"filters": {"pluginFilters": {"bitcrush": {"bits": 6}}}` |
//! | [`EventListener`] | [`EventCounter`]: counts every player event by type | (automatic) |
//! | HTTP routes | `GET /` (stats) and `POST /reset` | `GET /v1/plugins/example/` |
//!
//! Settings (all optional), read from `WAVYZON_PLUGIN_EXAMPLE_<KEY>`:
//!
//! | Variable | Default | Meaning |
//! | --- | --- | --- |
//! | `WAVYZON_PLUGIN_EXAMPLE_AMPLITUDE` | `0.2` | Tone loudness, `0.0` (exclusive) to `1.0` |
//! | `WAVYZON_PLUGIN_EXAMPLE_DEFAULT_SECONDS` | `10` | Length of `tone:<hz>` without an explicit length |
//! | `WAVYZON_PLUGIN_EXAMPLE_MAX_SECONDS` | `600` | Longest finite tone accepted |
//!
//! [`Source`]: wavyzon_plugin::Source
//! [`Playable`]: wavyzon_plugin::Playable
//! [`Format::Pcm48`]: wavyzon_plugin::Format::Pcm48
//! [`FilterFactory`]: wavyzon_plugin::FilterFactory
//! [`EventListener`]: wavyzon_plugin::EventListener

mod bitcrush;
mod counter;
mod tone;

use std::sync::Arc;

use wavyzon_plugin::axum::extract::State;
use wavyzon_plugin::axum::routing::{get, post};
use wavyzon_plugin::axum::{Json, Router};
use wavyzon_plugin::serde_json::{Value, json};
use wavyzon_plugin::{API_VERSION, Plugin, PluginContext, PluginError, PluginManifest, Registry, async_trait};

pub use bitcrush::{Bitcrush, BitcrushConfig, BitcrushFactory};
pub use counter::EventCounter;
pub use tone::{ToneReader, ToneSettings, ToneSource, ToneSpec};

/// The plugin's name: route prefix `/v1/plugins/example`, env prefix
/// `WAVYZON_PLUGIN_EXAMPLE_`.
pub const NAME: &str = "example";

/// State shared between the plugin's parts and its HTTP routes.
#[derive(Default)]
pub struct Shared {
    pub counter: EventCounter,
    pub tones: tone::ToneStats,
}

/// The plugin itself. Cheap to construct; all work happens in
/// [`Plugin::register`].
#[derive(Default, Clone)]
pub struct ExamplePlugin {
    shared: Arc<Shared>,
}

impl ExamplePlugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// The state the routes read (useful in tests).
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// Entry point used by the `wavyzon` binary when the plugin's cargo feature is
/// enabled.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ExamplePlugin::new())
}

#[async_trait]
impl Plugin for ExamplePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            description: "Sine-tone source, bitcrush filter, event counter and stats route",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let settings = ToneSettings::from_context(ctx)?;
        tracing::info!(
            plugin = NAME,
            ?settings,
            node = ctx.node_version,
            "registering example plugin"
        );

        reg.source(Arc::new(ToneSource::new(settings, self.shared.clone())))
            .filter(Arc::new(BitcrushFactory))
            .listener(Arc::new(self.clone()))
            .routes(routes(self.shared.clone()));
        Ok(())
    }

    async fn shutdown(&self) {
        tracing::info!(
            plugin = NAME,
            events = self.shared.counter.total(),
            "example plugin stopped"
        );
    }
}

/// The listener is the plugin itself, forwarding to the shared counter, so the
/// routes see the same numbers.
#[async_trait]
impl wavyzon_plugin::EventListener for ExamplePlugin {
    async fn on_event(&self, session_id: &str, event: &wavyzon_plugin::Event) {
        self.shared.counter.on_event(session_id, event).await;
    }
}

/// `GET /` returns counters, `POST /reset` clears them. Mounted by the node
/// at `/v1/plugins/example`, behind the node's `Authorization` check.
pub fn routes(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(stats))
        .route("/reset", post(reset))
        .with_state(shared)
}

async fn stats(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({
        "plugin": NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "events": shared.counter.snapshot(),
        "totalEvents": shared.counter.total(),
        "sessions": shared.counter.sessions(),
        "tonesOpened": shared.tones.opened(),
    }))
}

async fn reset(State(shared): State<Arc<Shared>>) -> Json<Value> {
    shared.counter.reset();
    Json(json!({ "reset": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use wavyzon_plugin::{Event, EventListener, EventType};

    fn ctx(settings: &[(&str, &str)]) -> PluginContext {
        PluginContext {
            settings: settings
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
            data_dir: PathBuf::from("/nonexistent"),
            node_version: "test",
        }
    }

    #[test]
    fn manifest_targets_current_api() {
        let m = ExamplePlugin::new().manifest();
        assert_eq!(m.name, "example");
        assert_eq!(m.api_version, API_VERSION);
    }

    #[tokio::test]
    async fn registers_every_extension_point() {
        let p = ExamplePlugin::new();
        let mut reg = Registry::new();
        reg.begin(NAME);
        p.register(&ctx(&[]), &mut reg).await.unwrap();
        assert_eq!(reg.sources().len(), 1);
        assert_eq!(reg.sources()[0].name(), "tone");
        assert_eq!(reg.filters().len(), 1);
        assert_eq!(reg.filters()[0].name(), "bitcrush");
        assert_eq!(reg.listeners().len(), 1);
        let routes = reg.take_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].0, NAME);
    }

    #[tokio::test]
    async fn rejects_bad_settings() {
        let mut reg = Registry::new();
        let err = ExamplePlugin::new()
            .register(&ctx(&[("amplitude", "3")]), &mut reg)
            .await;
        assert!(matches!(err, Err(PluginError::Config(_))));
    }

    #[tokio::test]
    async fn stats_route_reflects_events() {
        let p = ExamplePlugin::new();
        p.on_event("s1", &Event::new(EventType::TrackStartEvent, "1")).await;
        p.on_event("s1", &Event::new(EventType::PositionJump, "1")).await;
        p.on_event("s2", &Event::new(EventType::TrackStartEvent, "2")).await;

        let Json(v) = stats(State(p.shared().clone())).await;
        assert_eq!(v["totalEvents"], 3);
        assert_eq!(v["sessions"], 2);
        assert_eq!(v["events"]["TrackStartEvent"], 2);
        // wire names, not Rust names
        assert_eq!(v["events"]["PositionEvent"], 1);

        let Json(r) = reset(State(p.shared().clone())).await;
        assert_eq!(r["reset"], true);
        assert_eq!(p.shared().counter.total(), 0);
    }
}
