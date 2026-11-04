//! Compiled-in plugins, their selection with `WAVYZON_PLUGINS`, registration,
//! and the event bus that feeds their listeners.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use wavyzon_plugin::{API_VERSION, Event, EventListener, Plugin, PluginContext, PluginError, PluginManifest, Registry};

/// Every plugin compiled into this binary (one cargo feature each).
pub fn compiled_in() -> Vec<Arc<dyn Plugin>> {
    vec![
        #[cfg(feature = "plugin-example")]
        wavyzon_plugin_example::plugin(),
    ]
}

/// Why plugins could not be loaded. Every variant stops the node.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("WAVYZON_PLUGINS names {0:?}, which is not compiled into this binary (available: {1})")]
    Unknown(String, String),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error("plugin {0} data directory {1}: {2}")]
    DataDir(String, String, std::io::Error),
}

/// The plugins that registered, and what they contributed.
pub struct Loaded {
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub manifests: Vec<PluginManifest>,
    pub registry: Registry,
}

/// Pick plugins from `available` by `names` (empty = all, in their order),
/// check their API version, and register them in the order picked.
pub async fn load(
    available: Vec<Arc<dyn Plugin>>,
    names: &[String],
    data_root: &Path,
    node_version: &'static str,
) -> Result<Loaded, LoadError> {
    let selected: Vec<Arc<dyn Plugin>> = if names.is_empty() {
        available
    } else {
        let mut out = Vec::new();
        for n in names {
            let Some(p) = available.iter().find(|p| p.manifest().name.eq_ignore_ascii_case(n)) else {
                let list: Vec<&str> = available.iter().map(|p| p.manifest().name).collect();
                let list = if list.is_empty() {
                    "none".to_string()
                } else {
                    list.join(", ")
                };
                return Err(LoadError::Unknown(n.clone(), list));
            };
            if !out.iter().any(|q: &Arc<dyn Plugin>| Arc::ptr_eq(q, p)) {
                out.push(p.clone());
            }
        }
        out
    };
    let mut registry = Registry::new();
    let mut manifests = Vec::new();
    for p in &selected {
        let m = p.manifest();
        if m.api_version != API_VERSION {
            return Err(PluginError::ApiVersion {
                plugin: m.name.into(),
                found: m.api_version,
                expected: API_VERSION,
            }
            .into());
        }
        let ctx = PluginContext::from_env(m.name, data_root, node_version);
        std::fs::create_dir_all(&ctx.data_dir)
            .map_err(|e| LoadError::DataDir(m.name.into(), ctx.data_dir.display().to_string(), e))?;
        registry.begin(m.name);
        p.register(&ctx, &mut registry).await?;
        tracing::info!(plugin = m.name, version = m.version, "plugin registered");
        manifests.push(m);
    }
    Ok(Loaded {
        plugins: selected,
        manifests,
        registry,
    })
}

/// Delivers every player event to every plugin listener. Each listener runs
/// on its own task with an unbounded queue, so a slow one only delays itself.
#[derive(Clone, Default)]
pub struct EventBus {
    queues: Arc<Vec<UnboundedSender<(Arc<str>, Arc<Event>)>>>,
}

impl EventBus {
    /// Spawn one task per listener on `rt`.
    pub fn new(listeners: &[Arc<dyn EventListener>], rt: &tokio::runtime::Handle) -> Self {
        let mut queues = Vec::new();
        for l in listeners {
            let (tx, mut rx) = unbounded_channel::<(Arc<str>, Arc<Event>)>();
            let l = l.clone();
            rt.spawn(async move {
                while let Some((session, event)) = rx.recv().await {
                    l.on_event(&session, &event).await;
                }
            });
            queues.push(tx);
        }
        Self {
            queues: Arc::new(queues),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    pub fn publish(&self, session_id: &str, event: &Event) {
        if self.queues.is_empty() {
            return;
        }
        let session: Arc<str> = session_id.into();
        let event = Arc::new(event.clone());
        for q in self.queues.iter() {
            let _ = q.send((session.clone(), event.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavyzon_plugin::{PluginManifest, async_trait};

    struct Fake(&'static str, u32);

    #[async_trait]
    impl Plugin for Fake {
        fn manifest(&self) -> PluginManifest {
            PluginManifest {
                name: self.0,
                version: "1",
                description: "",
                api_version: self.1,
            }
        }
        async fn register(&self, _: &PluginContext, _: &mut Registry) -> Result<(), PluginError> {
            Ok(())
        }
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("wavyzon-plugins-test-{}", rand::random::<u64>()))
    }

    #[tokio::test]
    async fn selects_checks_and_creates_data_dirs() {
        let dir = tmp();
        let all =
            || -> Vec<Arc<dyn Plugin>> { vec![Arc::new(Fake("a", API_VERSION)), Arc::new(Fake("b", API_VERSION))] };
        let l = load(all(), &[], &dir, "t").await.unwrap();
        assert_eq!(l.manifests.iter().map(|m| m.name).collect::<Vec<_>>(), ["a", "b"]);
        assert!(dir.join("plugins/a").is_dir());
        let l = load(all(), &["b".into()], &dir, "t").await.unwrap();
        assert_eq!(l.manifests.len(), 1);
        assert!(matches!(
            load(all(), &["zzz".into()], &dir, "t").await,
            Err(LoadError::Unknown(..))
        ));
        let bad: Vec<Arc<dyn Plugin>> = vec![Arc::new(Fake("old", API_VERSION + 1))];
        assert!(matches!(
            load(bad, &[], &dir, "t").await,
            Err(LoadError::Plugin(PluginError::ApiVersion { .. }))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn example_plugin_is_compiled_in_by_default() {
        #[cfg(feature = "plugin-example")]
        assert!(compiled_in().iter().any(|p| p.manifest().name == "example"));
    }
}
