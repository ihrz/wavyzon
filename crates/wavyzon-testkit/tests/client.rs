//! `TestClient` against a tiny fake Lavalink v4 server.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use wavyzon_plugin::EventType;
use wavyzon_testkit::client::{Method, VoiceState};
use wavyzon_testkit::{TestClient, TestClientError};

const PW: &str = "pw";
const T: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct App {
    tx: broadcast::Sender<String>,
}

fn authed(h: &HeaderMap) -> bool {
    h.get("Authorization").and_then(|v| v.to_str().ok()) == Some(PW)
}

async fn websocket(State(app): State<App>, h: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
    }
    if h.get("User-Id").is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let resumed = h.get("Session-Id").is_some();
    let mut rx = app.tx.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        let ready = json!({ "op": "ready", "resumed": resumed, "sessionId": "s1" }).to_string();
        if socket.send(Message::Text(ready.into())).await.is_err() {
            return;
        }
        while let Ok(m) = rx.recv().await {
            if socket.send(Message::Text(m.into())).await.is_err() {
                return;
            }
        }
    })
}

async fn load(h: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({ "loadType": "search", "data": [{ "encoded": format!("enc-{}", q["identifier"]) }] })).into_response()
}

async fn update(
    State(app): State<App>,
    Path((sid, guild)): Path<(String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let _ = app
        .tx
        .send(json!({ "op": "playerUpdate", "guildId": guild, "state": { "position": 0 } }).to_string());
    for kind in ["TrackLoadingEvent", "TrackStartEvent"] {
        let _ = app
            .tx
            .send(json!({ "op": "event", "type": kind, "guildId": guild }).to_string());
    }
    Json(json!({ "session": sid, "guildId": guild, "body": body, "noReplace": q.get("noReplace") }))
}

async fn destroy(State(app): State<App>, Path((_sid, guild)): Path<(String, String)>) -> StatusCode {
    let _ = app
        .tx
        .send(json!({ "op": "event", "type": "TrackEndEvent", "guildId": guild, "reason": "cleanup" }).to_string());
    StatusCode::NO_CONTENT
}

async fn server() -> String {
    let (tx, _) = broadcast::channel(64);
    let app = Router::new()
        .route("/v4/websocket", get(websocket))
        .route("/v4/loadtracks", get(load))
        .route("/v4/sessions/{sid}/players/{guild}", patch(update).delete(destroy))
        .with_state(App { tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn connects_and_waits_for_events() {
    let base = server().await;
    let c = TestClient::connect(&base, PW, "123").await.unwrap();
    assert_eq!(c.session_id(), "s1");
    assert!(!c.resumed());

    let res = c.load_tracks("tonesearch:440 3000").await.unwrap();
    assert_eq!(res["data"][0]["encoded"], "enc-tonesearch:440 3000");

    let echo = c.play("42", "enc").await.unwrap();
    assert_eq!(echo["body"]["track"]["encoded"], "enc");
    assert_eq!(echo["session"], "s1");
    let up = c.wait_for_player_update("42", T).await.unwrap();
    assert_eq!(up["state"]["position"], 0);
    // Cursor semantics: loading comes before start, and each is seen once.
    c.wait_for_event("TrackLoadingEvent", T).await.unwrap();
    let start = c
        .wait_for_guild_event("42", EventType::TrackStartEvent, T)
        .await
        .unwrap();
    assert_eq!(start["guildId"], "42");
    let again = c
        .wait_for_event(EventType::TrackStartEvent, Duration::from_millis(100))
        .await;
    assert!(matches!(again, Err(TestClientError::Timeout { .. })));

    let echo = c.update_player("42", json!({ "paused": true }), true).await.unwrap();
    assert_eq!(echo["noReplace"], "true");
    let voice = VoiceState {
        token: "t".into(),
        endpoint: "ws://127.0.0.1:1".into(),
        session_id: "v".into(),
        channel_id: "9".into(),
    };
    let echo = c.voice_update("42", &voice).await.unwrap();
    assert_eq!(echo["body"]["voice"]["endpoint"], "ws://127.0.0.1:1");

    assert_eq!(c.destroy_player("42").await.unwrap(), 204);
    let end = c.wait_for_event(EventType::TrackEndEvent, T).await.unwrap();
    assert_eq!(end["reason"], "cleanup");
    assert_eq!(c.typed_events().len(), c.events().len());
    assert_eq!(c.player_updates("42").len(), 3);
}

#[tokio::test]
async fn auth_failures_and_resume_header() {
    let base = server().await;
    match TestClient::connect(&base, "wrong", "1").await {
        Err(TestClientError::Status { status, body }) => {
            assert_eq!(status, 401);
            assert_eq!(body["error"], "unauthorized");
        }
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("connected with a wrong password"),
    }
    let c = TestClient::builder(&base)
        .password(PW)
        .user_id("5")
        .resume("s1")
        .connect()
        .await
        .unwrap();
    assert!(c.resumed());
    let (status, _) = c.request(Method::GET, "loadtracks?identifier=x", None).await.unwrap();
    assert_eq!(status, 200);
    c.close().await;
    assert!(c.closed().is_some());
}
