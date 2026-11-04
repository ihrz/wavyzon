//! [`TestClient`]: a minimal Lavalink-style client for end-to-end tests of a
//! Wavyzon server.
//!
//! It opens the WebSocket (`/<api>/websocket` with `Authorization`,
//! `User-Id`, `Client-Name` and optionally `Session-Id` headers), waits for
//! `ready`, records every message, and wraps the REST routes.
//!
//! ```no_run
//! # async fn demo() -> Result<(), wavyzon_testkit::TestClientError> {
//! use std::time::Duration;
//! use serde_json::json;
//! use wavyzon_plugin::EventType;
//! use wavyzon_testkit::TestClient;
//!
//! let c = TestClient::connect("http://127.0.0.1:2333", "youshallnotpass", "123").await?;
//! let res = c.load_tracks("tonesearch:440:3000").await?;
//! let encoded = res["data"][0]["encoded"].as_str().unwrap().to_string();
//! c.update_player("42", json!({ "track": { "encoded": encoded } }), false).await?;
//! let start = c.wait_for_event(EventType::TrackStartEvent, Duration::from_secs(5)).await?;
//! assert_eq!(start["guildId"], "42");
//! c.destroy_player("42").await?;
//! # Ok(()) }
//! ```
//!
//! Waits are *cursor based*: each `wait_for_*` scans messages after the
//! previous match, so consecutive waits observe consecutive events.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use wavyzon_plugin::{Event, EventType};

/// HTTP method type used by [`TestClient::request`].
pub use reqwest::Method;

/// Errors of [`TestClient`].
#[derive(Debug, thiserror::Error)]
pub enum TestClientError {
    /// HTTP transport error.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// WebSocket error.
    #[error("websocket: {0}")]
    Ws(#[from] Box<tungstenite::Error>),
    /// Non-2xx response (REST or WebSocket upgrade).
    #[error("HTTP {status}: {body}")]
    Status {
        /// Status code.
        status: u16,
        /// Response body (JSON if it parsed, else a string).
        body: Value,
    },
    /// Nothing matched in time.
    #[error("timed out after {timeout:?} waiting for {what}; last messages: {recent}")]
    Timeout {
        /// What was awaited.
        what: String,
        /// How long.
        timeout: Duration,
        /// The last few messages seen, for diagnosis.
        recent: String,
    },
    /// The server did something unexpected.
    #[error("protocol: {0}")]
    Protocol(String),
}

/// Something naming an event type: `"TrackStartEvent"` or
/// [`EventType::TrackStartEvent`].
pub trait EventKind {
    /// The wire name (`type` field).
    fn event_name(&self) -> String;
}

impl EventKind for &str {
    fn event_name(&self) -> String {
        (*self).to_string()
    }
}

impl EventKind for String {
    fn event_name(&self) -> String {
        self.clone()
    }
}

impl EventKind for EventType {
    fn event_name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

/// Voice state to send in a player update (`voice` field).
#[derive(Debug, Clone, Default)]
pub struct VoiceState {
    /// Voice token from `VOICE_SERVER_UPDATE`.
    pub token: String,
    /// Endpoint (for tests: [`MockVoiceGateway::endpoint`](crate::MockVoiceGateway::endpoint)).
    pub endpoint: String,
    /// Session id from `VOICE_STATE_UPDATE`.
    pub session_id: String,
    /// Voice channel id.
    pub channel_id: String,
}

/// Configures a [`TestClient`].
#[derive(Debug, Clone)]
pub struct TestClientBuilder {
    base: String,
    password: String,
    user_id: String,
    client_name: String,
    api: String,
    resume: Option<String>,
    ready_timeout: Duration,
}

impl TestClientBuilder {
    /// The `Authorization` header (default `"youshallnotpass"`).
    pub fn password(mut self, p: &str) -> Self {
        self.password = p.into();
        self
    }

    /// The `User-Id` header (default `"1"`).
    pub fn user_id(mut self, id: &str) -> Self {
        self.user_id = id.into();
        self
    }

    /// The `Client-Name` header (default `"wavyzon-testkit/0.1"`).
    pub fn client_name(mut self, name: &str) -> Self {
        self.client_name = name.into();
        self
    }

    /// API prefix, `"v4"` (default, Lavalink) or `"v1"` (native).
    pub fn api(mut self, api: &str) -> Self {
        self.api = api.trim_matches('/').into();
        self
    }

    /// Send `Session-Id` to resume a session.
    pub fn resume(mut self, session_id: &str) -> Self {
        self.resume = Some(session_id.into());
        self
    }

    /// How long to wait for `ready` (default 5 s).
    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// Connect and wait for `ready`.
    pub async fn connect(self) -> Result<TestClient, TestClientError> {
        TestClient::open(self).await
    }
}

#[derive(Default)]
struct Shared {
    messages: Vec<Value>,
    cursor: usize,
    closed: Option<(Option<u16>, String)>,
}

/// A Lavalink-style test client. See the [module docs](self).
pub struct TestClient {
    http: reqwest::Client,
    base: String,
    api: String,
    password: String,
    user_id: String,
    session_id: String,
    resumed: bool,
    shared: Arc<Mutex<Shared>>,
    out: mpsc::UnboundedSender<Message>,
    task: AbortHandle,
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn ws_url(base: &str, api: &str) -> String {
    let base = base.trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .map(|r| format!("wss://{r}"))
        .or_else(|| base.strip_prefix("http://").map(|r| format!("ws://{r}")))
        .unwrap_or_else(|| base.to_string());
    format!("{rest}/{api}/websocket")
}

fn header(v: &str) -> Result<HeaderValue, TestClientError> {
    HeaderValue::from_str(v).map_err(|e| TestClientError::Protocol(format!("bad header value {v:?}: {e}")))
}

impl TestClient {
    /// A builder for a server at `base_url` (`http://127.0.0.1:2333`).
    pub fn builder(base_url: &str) -> TestClientBuilder {
        TestClientBuilder {
            base: base_url.trim_end_matches('/').into(),
            password: "youshallnotpass".into(),
            user_id: "1".into(),
            client_name: "wavyzon-testkit/0.1".into(),
            api: "v4".into(),
            resume: None,
            ready_timeout: Duration::from_secs(5),
        }
    }

    /// Connect to `base_url` on the `v4` API and wait for `ready`.
    pub async fn connect(base_url: &str, password: &str, user_id: &str) -> Result<Self, TestClientError> {
        Self::builder(base_url)
            .password(password)
            .user_id(user_id)
            .connect()
            .await
    }

    async fn open(b: TestClientBuilder) -> Result<Self, TestClientError> {
        let mut req = ws_url(&b.base, &b.api).into_client_request().map_err(Box::new)?;
        let h = req.headers_mut();
        h.insert("Authorization", header(&b.password)?);
        h.insert("User-Id", header(&b.user_id)?);
        h.insert("Client-Name", header(&b.client_name)?);
        if let Some(sid) = &b.resume {
            h.insert("Session-Id", header(sid)?);
        }
        let ws = match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => ws,
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp
                    .body()
                    .as_ref()
                    .map(|b| {
                        serde_json::from_slice(b).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(b).into()))
                    })
                    .unwrap_or(Value::Null);
                return Err(TestClientError::Status { status, body });
            }
            Err(e) => return Err(Box::new(e).into()),
        };
        let (mut sink, mut stream) = ws.split();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (out, mut rx) = mpsc::unbounded_channel::<Message>();
        let task = {
            let shared = shared.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        m = rx.recv() => match m {
                            Some(m) => {
                                let closing = matches!(m, Message::Close(_));
                                if sink.send(m).await.is_err() || closing {
                                    shared.lock().closed.get_or_insert((Some(1000), "closed by client".into()));
                                    break;
                                }
                            }
                            None => break,
                        },
                        m = stream.next() => match m {
                            Some(Ok(Message::Text(t))) => {
                                let v = serde_json::from_str(t.as_str()).unwrap_or_else(|_| Value::String(t.to_string()));
                                shared.lock().messages.push(v);
                            }
                            Some(Ok(Message::Close(f))) => {
                                shared.lock().closed = Some((f.as_ref().map(|f| u16::from(f.code)), f.map(|f| f.reason.to_string()).unwrap_or_default()));
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                shared.lock().closed = Some((None, e.to_string()));
                                break;
                            }
                            None => {
                                shared.lock().closed.get_or_insert((None, "stream ended".into()));
                                break;
                            }
                        },
                    }
                }
            })
        };
        let mut client = TestClient {
            http: reqwest::Client::new(),
            base: b.base,
            api: b.api,
            password: b.password,
            user_id: b.user_id,
            session_id: String::new(),
            resumed: false,
            shared,
            out,
            task: task.abort_handle(),
        };
        let ready = client.wait_for_op("ready", b.ready_timeout).await?;
        client.session_id = ready["sessionId"]
            .as_str()
            .ok_or_else(|| TestClientError::Protocol(format!("ready without sessionId: {ready}")))?
            .to_string();
        client.resumed = ready["resumed"].as_bool().unwrap_or(false);
        Ok(client)
    }

    // ----- session --------------------------------------------------------------

    /// The session id from `ready`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether `ready` said the session was resumed.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// The `User-Id` used.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The server base URL.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Whether the WebSocket was closed, and with what `(code, reason)`.
    pub fn closed(&self) -> Option<(Option<u16>, String)> {
        self.shared.lock().closed.clone()
    }

    /// Send a raw text frame.
    pub fn send_text(&self, text: &str) -> bool {
        self.out.send(Message::Text(text.into())).is_ok()
    }

    /// Close the WebSocket normally (the session may then be resumed).
    pub async fn close(&self) {
        let _ = self.out.send(Message::Close(None));
        let _ = crate::poll_until(Duration::from_secs(2), "close", || self.closed().is_some()).await;
    }

    // ----- messages -------------------------------------------------------------

    /// Every message received, in order.
    pub fn messages(&self) -> Vec<Value> {
        self.shared.lock().messages.clone()
    }

    /// Messages with `op == "event"`.
    pub fn events(&self) -> Vec<Value> {
        self.messages().into_iter().filter(|m| m["op"] == "event").collect()
    }

    /// Events that parse as [`wavyzon_plugin::Event`].
    pub fn typed_events(&self) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }

    /// `playerUpdate` messages for `guild_id`.
    pub fn player_updates(&self, guild_id: &str) -> Vec<Value> {
        self.messages()
            .into_iter()
            .filter(|m| m["op"] == "playerUpdate" && m["guildId"] == guild_id)
            .collect()
    }

    /// Forget all messages and reset the cursor.
    pub fn clear(&self) {
        let mut s = self.shared.lock();
        s.messages.clear();
        s.cursor = 0;
    }

    /// Move the wait cursor past everything received so far.
    pub fn skip_seen(&self) {
        let mut s = self.shared.lock();
        s.cursor = s.messages.len();
    }

    /// Wait (from the cursor) for a message matching `pred`, then move the
    /// cursor past it.
    pub async fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<Value, TestClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut s = self.shared.lock();
                let start = s.cursor;
                if let Some(i) = s.messages[start..].iter().position(&pred) {
                    s.cursor = start + i + 1;
                    return Ok(s.messages[start + i].clone());
                }
                if tokio::time::Instant::now() >= deadline || s.closed.is_some() {
                    let n = s.messages.len();
                    let recent = s.messages[n.saturating_sub(5)..]
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let what = match &s.closed {
                        Some(c) => format!("{what} (socket closed: {c:?})"),
                        None => what.to_string(),
                    };
                    return Err(TestClientError::Timeout { what, timeout, recent });
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait for a message with `op`.
    pub async fn wait_for_op(&self, op: &str, timeout: Duration) -> Result<Value, TestClientError> {
        self.wait_for(&format!("op {op}"), timeout, |m| m["op"] == op).await
    }

    /// Wait for an event of `kind` (any guild).
    pub async fn wait_for_event(&self, kind: impl EventKind, timeout: Duration) -> Result<Value, TestClientError> {
        let name = kind.event_name();
        self.wait_for(&format!("event {name}"), timeout, |m| {
            m["op"] == "event" && m["type"] == name.as_str()
        })
        .await
    }

    /// Wait for an event of `kind` in `guild_id`.
    pub async fn wait_for_guild_event(
        &self,
        guild_id: &str,
        kind: impl EventKind,
        timeout: Duration,
    ) -> Result<Value, TestClientError> {
        let name = kind.event_name();
        self.wait_for(&format!("event {name} in {guild_id}"), timeout, |m| {
            m["op"] == "event" && m["type"] == name.as_str() && m["guildId"] == guild_id
        })
        .await
    }

    /// Wait for a `playerUpdate` of `guild_id`.
    pub async fn wait_for_player_update(&self, guild_id: &str, timeout: Duration) -> Result<Value, TestClientError> {
        self.wait_for(&format!("playerUpdate {guild_id}"), timeout, |m| {
            m["op"] == "playerUpdate" && m["guildId"] == guild_id
        })
        .await
    }

    // ----- REST -----------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{path}", self.base)
        } else {
            format!("{}/{}/{path}", self.base, self.api)
        }
    }

    /// Raw request. `path` is relative to the API prefix (`"info"` →
    /// `/v4/info`) unless it starts with `/`. Returns status and body (JSON,
    /// a JSON string for non-JSON bodies, `Null` when empty).
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), TestClientError> {
        let mut req = self
            .http
            .request(method, self.url(path))
            .header("Authorization", &self.password);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        Ok((status, body))
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, TestClientError> {
        let (status, body) = self.request(method, path, body).await?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(TestClientError::Status { status, body })
        }
    }

    fn player_path(&self, guild_id: &str) -> String {
        format!("sessions/{}/players/{guild_id}", self.session_id)
    }

    /// `GET /<api>/info`.
    pub async fn info(&self) -> Result<Value, TestClientError> {
        self.ok(Method::GET, "info", None).await
    }

    /// `GET /<api>/stats`.
    pub async fn stats(&self) -> Result<Value, TestClientError> {
        self.ok(Method::GET, "stats", None).await
    }

    /// `GET /<api>/loadtracks?identifier=…`.
    pub async fn load_tracks(&self, identifier: &str) -> Result<Value, TestClientError> {
        let res = self
            .http
            .get(self.url("loadtracks"))
            .query(&[("identifier", identifier)])
            .header("Authorization", &self.password)
            .send()
            .await?;
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(TestClientError::Status { status, body })
        }
    }

    /// `GET /<api>/decodetrack?encodedTrack=…`.
    pub async fn decode_track(&self, encoded: &str) -> Result<Value, TestClientError> {
        self.ok(Method::GET, &format!("decodetrack?encodedTrack={encoded}"), None)
            .await
    }

    /// `GET /<api>/sessions/<sid>/players`.
    pub async fn players(&self) -> Result<Value, TestClientError> {
        self.ok(Method::GET, &format!("sessions/{}/players", self.session_id), None)
            .await
    }

    /// `GET /<api>/sessions/<sid>/players/<guild>`.
    pub async fn get_player(&self, guild_id: &str) -> Result<Value, TestClientError> {
        self.ok(Method::GET, &self.player_path(guild_id), None).await
    }

    /// `PATCH /<api>/sessions/<sid>/players/<guild>[?noReplace=true]` with `body`.
    pub async fn update_player(&self, guild_id: &str, body: Value, no_replace: bool) -> Result<Value, TestClientError> {
        let mut path = self.player_path(guild_id);
        if no_replace {
            path.push_str("?noReplace=true");
        }
        self.ok(Method::PATCH, &path, Some(body)).await
    }

    /// Play `encoded` (a player update with `track.encoded`).
    pub async fn play(&self, guild_id: &str, encoded: &str) -> Result<Value, TestClientError> {
        self.update_player(guild_id, json!({ "track": { "encoded": encoded } }), false)
            .await
    }

    /// Send voice state (a player update with `voice`).
    pub async fn voice_update(&self, guild_id: &str, voice: &VoiceState) -> Result<Value, TestClientError> {
        let body = json!({ "voice": {
            "token": voice.token,
            "endpoint": voice.endpoint,
            "sessionId": voice.session_id,
            "channelId": voice.channel_id,
        }});
        self.update_player(guild_id, body, false).await
    }

    /// `DELETE /<api>/sessions/<sid>/players/<guild>`; returns the status.
    pub async fn destroy_player(&self, guild_id: &str) -> Result<u16, TestClientError> {
        let (status, body) = self.request(Method::DELETE, &self.player_path(guild_id), None).await?;
        if (200..300).contains(&status) {
            Ok(status)
        } else {
            Err(TestClientError::Status { status, body })
        }
    }

    /// `PATCH /<api>/sessions/<sid>` (resuming, timeout…).
    pub async fn update_session(&self, body: Value) -> Result<Value, TestClientError> {
        self.ok(Method::PATCH, &format!("sessions/{}", self.session_id), Some(body))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_names() {
        assert_eq!(
            ws_url("http://127.0.0.1:2333/", "v4"),
            "ws://127.0.0.1:2333/v4/websocket"
        );
        assert_eq!(ws_url("https://node.test", "v1"), "wss://node.test/v1/websocket");
        assert_eq!(EventType::TrackStartEvent.event_name(), "TrackStartEvent");
        assert_eq!(EventType::PositionJump.event_name(), "PositionEvent");
        assert_eq!("X".event_name(), "X");
    }
}
