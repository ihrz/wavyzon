//! # wavyzon-plugin-apo
//!
//! Equalizer APO configuration files as a Wavyzon playback filter: the EQ
//! presets people already have (AutoEQ headphone corrections, REW exports,
//! hand-written configs) applied to what a bot plays.
//!
//! ```json
//! { "filters": { "pluginFilters": { "apo": { "preset": "headphones/hd600" } } } }
//! { "filters": { "pluginFilters": { "apo": { "config": "Preamp: -6 dB\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1.41" } } } }
//! ```
//!
//! Supported: `Preamp`, `Filter` (PK/Modal/PEQ, LP, LPQ, HP, HPQ, BP, NO, AP,
//! LS/LSC, HS/HSC with a slope, LS/HS 6dB and 12dB, IIR), `GraphicEQ`
//! (minimum-phase FIR), `Channel`, `Copy`, `Delay`, `Include`, `Convolution`
//! (impulse responses: convolution reverb, room correction) and, as a
//! Wavyzon extension APO ignores, `Reverb: Mix 30 % Room 70 % Damping 50 %`.
//! `Device`, `Stage`, `If` blocks, `Eval` and inline expressions are skipped
//! with a warning.
//!
//! Presets are the `.txt` files under the presets directory
//! (`WAVYZON_PLUGIN_APO_DIR`, default `$WAVYZON_DATA_DIR/plugins/apo`),
//! loaded with their impulse responses at startup and on
//! `POST /v1/plugins/apo/reload`. Routes:
//!
//! | Route | |
//! | --- | --- |
//! | `GET /v1/plugins/apo/` | presets, impulse responses, warnings |
//! | `POST /v1/plugins/apo/reload` | re-read the directory |
//! | `POST /v1/plugins/apo/check` | compile `{"config": "..."}` and report warnings |

mod conv;
mod design;
mod ir;
mod parse;
mod presets;
mod program;
mod reverb;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use wavyzon_plugin::axum::extract::State;
use wavyzon_plugin::axum::http::StatusCode;
use wavyzon_plugin::axum::routing::{get, post};
use wavyzon_plugin::axum::{Json, Router};
use wavyzon_plugin::serde_json::{Value, json};
use wavyzon_plugin::{
    API_VERSION, AudioFilter, Failure, FilterFactory, Plugin, PluginContext, PluginError, PluginManifest, Registry,
    async_trait,
};

pub use parse::{Command, Parsed, parse};
pub use presets::Store;
pub use program::{Engine, Program, compile};

/// Plugin and filter name: `pluginFilters.apo`, `/v1/plugins/apo`,
/// `WAVYZON_PLUGIN_APO_*`.
pub const NAME: &str = "apo";

/// State shared by the filter factory and the routes.
pub struct Shared {
    store: ArcSwap<Store>,
    dir: OnceLock<PathBuf>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            store: ArcSwap::from_pointee(Store::default()),
            dir: OnceLock::new(),
        }
    }
}

impl Shared {
    pub fn store(&self) -> Arc<Store> {
        self.store.load_full()
    }

    /// Re-read the presets directory on a blocking thread and swap it in.
    pub async fn reload(&self) -> Arc<Store> {
        let Some(dir) = self.dir.get().cloned() else {
            return self.store();
        };
        let store = match tokio::task::spawn_blocking(move || Store::load(&dir)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(plugin = NAME, error = %e, "loading APO presets panicked");
                return self.store();
            }
        };
        log_store(&store);
        self.store.store(store.clone());
        store
    }
}

fn log_store(store: &Store) {
    let broken = store.presets.values().filter(|p| p.program.is_err()).count();
    tracing::info!(
        plugin = NAME,
        dir = %store.dir.display(),
        presets = store.presets.len(),
        broken,
        impulses = store.impulses().len(),
        "APO presets loaded"
    );
    for w in &store.warnings {
        tracing::warn!(plugin = NAME, warning = %w, "APO presets directory");
    }
    for (name, p) in &store.presets {
        match &p.program {
            Err(e) => tracing::warn!(plugin = NAME, preset = %name, error = %e, "APO preset cannot be used"),
            Ok(prog) if !prog.warnings.is_empty() => {
                tracing::warn!(plugin = NAME, preset = %name, warnings = ?prog.warnings, "APO preset has skipped lines")
            }
            Ok(_) => {}
        }
    }
}

/// The plugin.
#[derive(Default, Clone)]
pub struct ApoPlugin {
    shared: Arc<Shared>,
}

impl ApoPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// Entry point used by the `wavyzon` binary (cargo feature `plugin-apo`).
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ApoPlugin::new())
}

#[async_trait]
impl Plugin for ApoPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            description: "Equalizer APO configurations (EQ, GraphicEQ, convolution, reverb) as a playback filter",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let dir = match ctx.setting("dir") {
            Some(d) if !d.trim().is_empty() => {
                let d = PathBuf::from(d.trim());
                if !d.is_dir() {
                    return Err(PluginError::Config(format!(
                        "WAVYZON_PLUGIN_APO_DIR={} is not a directory",
                        d.display()
                    )));
                }
                d
            }
            _ => ctx.data_dir.clone(),
        };
        let _ = self.shared.dir.set(dir);
        self.shared.reload().await;
        reg.filter(Arc::new(ApoFactory {
            shared: self.shared.clone(),
        }))
        .routes(routes(self.shared.clone()));
        Ok(())
    }
}

/// Builds an [`Engine`] from `{"preset": name}` or `{"config": text}`.
pub struct ApoFactory {
    shared: Arc<Shared>,
}

fn invalid(msg: impl Into<String>) -> Failure {
    Failure::common(NAME, "invalid apo filter", Some(msg.into()))
}

impl ApoFactory {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// The program a filter configuration asks for. Compiling inline text
    /// only reads the in-memory store.
    pub fn program(&self, config: &Value) -> Result<Arc<Program>, Failure> {
        let obj = config
            .as_object()
            .ok_or_else(|| invalid(r#"expected {"preset": "<name>"} or {"config": "<APO text>"}"#))?;
        if let Some(k) = obj.keys().find(|k| *k != "preset" && *k != "config") {
            return Err(invalid(format!("unknown field {k:?}")));
        }
        let store = self.shared.store();
        match (obj.get("preset"), obj.get("config")) {
            (Some(p), None) => {
                let name = p.as_str().ok_or_else(|| invalid("preset must be a string"))?;
                let preset = store
                    .preset(name)
                    .ok_or_else(|| invalid(format!("no preset named {name:?}")))?;
                preset
                    .program
                    .clone()
                    .map_err(|e| invalid(format!("preset {name:?}: {e}")))
            }
            (None, Some(c)) => {
                let text = c.as_str().ok_or_else(|| invalid("config must be a string"))?;
                compile(text, "", &*store).map(Arc::new).map_err(invalid)
            }
            (Some(_), Some(_)) => Err(invalid("give either preset or config, not both")),
            (None, None) => Err(invalid(r#"expected {"preset": "<name>"} or {"config": "<APO text>"}"#)),
        }
    }
}

impl FilterFactory for ApoFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        Ok(Box::new(Engine::new(self.program(config)?)))
    }
}

fn store_json(store: &Store) -> Value {
    let presets: Vec<Value> = store
        .presets
        .iter()
        .map(|(name, p)| match &p.program {
            Ok(prog) => json!({
                "name": name,
                "path": p.path,
                "stages": prog.summary(),
                "warnings": prog.warnings,
            }),
            Err(e) => json!({ "name": name, "path": p.path, "error": e }),
        })
        .collect();
    let impulses: Vec<Value> = store
        .impulses()
        .into_iter()
        .map(|(path, ir)| match ir {
            Ok(ir) => json!({
                "path": path,
                "channels": ir.channels.len(),
                "seconds": ir.frames as f64 / 48_000.0,
                "sourceRate": ir.source_rate,
                "truncated": ir.truncated,
                "droppedChannels": ir.dropped_channels,
            }),
            Err(e) => json!({ "path": path, "error": e }),
        })
        .collect();
    json!({
        "dir": store.dir.display().to_string(),
        "presets": presets,
        "impulses": impulses,
        "warnings": store.warnings,
    })
}

/// Mounted at `/v1/plugins/apo` behind the node's authorization.
pub fn routes(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/reload", post(reload))
        .route("/check", post(check))
        .with_state(shared)
}

async fn list(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(store_json(&shared.store()))
}

async fn reload(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let store = shared.reload().await;
    Json(store_json(&store))
}

async fn check(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let factory = ApoFactory::new(shared);
    match factory.program(&body) {
        Ok(p) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "stages": p.summary(), "warnings": p.warnings })),
        ),
        Err(f) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": if f.cause.is_empty() { f.message } else { f.cause } })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::presets::tests::temp_dir;
    use wavyzon_plugin::FRAME_SAMPLES;

    fn ctx(dir: &std::path::Path, settings: &[(&str, &str)]) -> PluginContext {
        PluginContext {
            settings: settings
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
            data_dir: dir.to_path_buf(),
            node_version: "test",
        }
    }

    #[tokio::test]
    async fn registers_a_filter_and_routes_and_builds_engines() {
        let d = temp_dir("plugin");
        std::fs::write(d.join("loud.txt"), "Preamp: 6.0206 dB").unwrap();
        let p = ApoPlugin::new();
        let mut reg = Registry::new();
        reg.begin(NAME);
        p.register(&ctx(&d, &[]), &mut reg).await.unwrap();
        assert_eq!(reg.filters().len(), 1);
        assert_eq!(reg.filters()[0].name(), "apo");
        assert_eq!(reg.take_routes().len(), 1);

        let f = &reg.filters()[0];
        let mut e = f.create(&json!({"preset": "loud"})).unwrap();
        let mut frame = vec![0.25f32; FRAME_SAMPLES];
        e.process(&mut frame);
        assert!((frame[0] - 0.5).abs() < 1e-4);
        assert!(
            f.create(&json!({"config": "Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1"}))
                .is_ok()
        );

        for bad in [
            json!(null),
            json!({}),
            json!({"preset": "nope"}),
            json!({"preset": 1}),
            json!({"preset": "loud", "config": ""}),
            json!({"config": "Include: nope.txt"}),
            json!({"preset": "loud", "mix": 1}),
        ] {
            assert!(f.create(&bad).is_err(), "{bad}");
        }

        // A new file shows up after a reload.
        std::fs::write(d.join("quiet.txt"), "Preamp: -6 dB").unwrap();
        assert!(f.create(&json!({"preset": "quiet"})).is_err());
        let Json(v) = reload(State(p.shared().clone())).await;
        assert_eq!(v["presets"].as_array().unwrap().len(), 2);
        assert!(f.create(&json!({"preset": "quiet"})).is_ok());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn check_route_reports_warnings_and_errors() {
        let d = temp_dir("check");
        let p = ApoPlugin::new();
        let mut reg = Registry::new();
        p.register(&ctx(&d, &[]), &mut reg).await.unwrap();
        let (code, Json(v)) = check(
            State(p.shared().clone()),
            Json(json!({"config": "Preamp: -3 dB\nDevice: Speakers\nFilter: ON PK Fc 100 Hz Gain 2 dB Q 1"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["stages"], json!(["preamp", "filter"]));
        assert_eq!(v["warnings"].as_array().unwrap().len(), 1);
        let (code, Json(v)) = check(State(p.shared().clone()), Json(json!({"config": "Delay: 5000 ms"}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("delay"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn a_missing_configured_directory_is_a_config_error() {
        let p = ApoPlugin::new();
        let mut reg = Registry::new();
        let d = std::env::temp_dir();
        let r = p
            .register(&ctx(&d, &[("dir", "/nonexistent/wavyzon-apo-presets")]), &mut reg)
            .await;
        assert!(matches!(r, Err(PluginError::Config(_))));
    }
}
