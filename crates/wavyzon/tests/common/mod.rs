//! A real node on `127.0.0.1:0` with test sources registered as a plugin.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use wavyzon::session::Manager;
use wavyzon::{Config, Node};
use wavyzon_plugin::{
    API_VERSION, Plugin, PluginContext, PluginError, PluginManifest, Registry, Severity, async_trait,
};
use wavyzon_testkit::{FailAt, FailingSource, TestClient, ToneSource};

pub const PASSWORD: &str = "pw";
pub const WAIT: Duration = Duration::from_secs(10);

/// Registers the testkit's sources: `testtone` (prefix `testtonesearch`,
/// identifiers `<hz>:<ms>`) and `failing` (fails on open).
pub struct TestSources {
    pub tone: Arc<ToneSource>,
}

#[async_trait]
impl Plugin for TestSources {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: "testkit",
            version: "0.0.0",
            description: "test sources",
            api_version: API_VERSION,
        }
    }
    async fn register(&self, _: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        reg.source(self.tone.clone());
        reg.source(Arc::new(FailingSource::new(Severity::Common).at(FailAt::Open)));
        Ok(())
    }
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let p = std::env::temp_dir().join(format!("wavyzon-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct TestNode {
    pub base: String,
    pub sessions: Arc<Manager>,
    pub tone: Arc<ToneSource>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    pub dir: Arc<TempDir>,
}

impl TestNode {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn client(&self, api: &str) -> TestClient {
        TestClient::builder(&self.base)
            .password(PASSWORD)
            .user_id("123")
            .api(api)
            .connect()
            .await
            .unwrap()
    }

    /// Stop gracefully (saves the state file when configured).
    pub async fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.task.take() {
            t.await.unwrap().unwrap();
        }
    }
}

pub fn config(dir: &TempDir, extra: &[(&str, &str)]) -> Config {
    let mut env: HashMap<String, String> = [
        ("WAVYZON_PASSWORD", PASSWORD),
        ("WAVYZON_ADDRESS", "127.0.0.1:0"),
        ("WAVYZON_UPDATE_INTERVAL_MS", "250"),
        ("WAVYZON_BUFFER_MS", "1000"),
        ("SOUNDCLOUD_ENABLED", "false"),
        ("DEEZER_ENABLED", "false"),
        ("SPOTIFY_ENABLED", "false"),
        ("APPLE_MUSIC_ENABLED", "false"),
        // hermetic: lyrics tests point LYRICS_API_URL at a local server
        ("LYRICS_ENABLED", "false"),
        ("WAVYZON_SEARCH_ORDER", "testtone"),
        ("WAVYZON_PLAYBACK_SOURCES", "testtone"),
        // no offline analysis of long test tones (it would outlive the test)
        ("WAVYZON_NORMALIZE", "false"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    env.insert("WAVYZON_DATA_DIR".into(), dir.0.join("data").display().to_string());
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    Config::load(&move |k| env.get(k).cloned()).unwrap()
}

pub fn test_plugins(tone: Arc<ToneSource>) -> Vec<Arc<dyn Plugin>> {
    let mut v: Vec<Arc<dyn Plugin>> = vec![Arc::new(TestSources { tone })];
    v.extend(wavyzon::plugins::compiled_in());
    v
}

pub async fn start(extra: &[(&str, &str)]) -> TestNode {
    start_in(Arc::new(TempDir::new()), extra).await
}

pub async fn start_in(dir: Arc<TempDir>, extra: &[(&str, &str)]) -> TestNode {
    let tone = Arc::new(
        ToneSource::new()
            .with_name("testtone")
            .with_search_prefix(Some("testtonesearch")),
    );
    let node = Node::build(config(&dir, extra), test_plugins(tone.clone()))
        .await
        .unwrap();
    node.restore().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let sessions = node.sessions.clone();
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(node.serve(listener, async move {
        let _ = rx.await;
    }));
    TestNode {
        base,
        sessions,
        tone,
        stop: Some(stop),
        task: Some(task),
        dir,
    }
}

/// Raw REST call with the password.
pub async fn call(method: reqwest::Method, url: &str, body: Option<Value>) -> (u16, Value) {
    call_with(method, url, body, Some(PASSWORD)).await
}

pub async fn call_with(method: reqwest::Method, url: &str, body: Option<Value>, auth: Option<&str>) -> (u16, Value) {
    let mut req = reqwest::Client::new().request(method, url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.bytes().await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, v)
}

pub fn tone_encoded(hz: u32, ms: i64) -> String {
    wavyzon_testkit::sources::test_track("testtone", &format!("{hz}:{ms}"), ms).encode()
}
