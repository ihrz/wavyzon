//! The client WebSocket (port of Cadence's `internal/api/ws.go`).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde_json::json;

use super::{Api, ApiError, St};
use crate::session::Protocol;

const PING_INTERVAL: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn websocket_v1(
    State(api): St,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    websocket(api, headers, upgrade, Protocol::V1)
}

pub(super) async fn websocket_v4(
    State(api): St,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    websocket(api, headers, upgrade, Protocol::V4)
}

fn header(h: &HeaderMap, name: &str) -> String {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn websocket(
    api: Arc<Api>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    protocol: Protocol,
) -> Response {
    let user_id = header(&headers, "User-Id");
    if user_id.is_empty() {
        return ApiError::bad_request("User-Id header is required").into_response();
    }
    let upgrade = match upgrade {
        Ok(u) => u,
        Err(e) => return ApiError::bad_request(e.to_string()).into_response(),
    };
    let client = header(&headers, "Client-Name");
    let resume = header(&headers, "Session-Id");
    upgrade.on_upgrade(move |socket| serve(api, socket, protocol, user_id, client, resume))
}

async fn serve(api: Arc<Api>, socket: WebSocket, protocol: Protocol, user_id: String, client: String, resume: String) {
    let mut attached = None;
    if !resume.is_empty()
        && let Some(c) = api.sessions.resume(&resume, &user_id)
        && c.protocol == protocol
        && let Ok(a) = c.attach()
    {
        attached = Some((c, a));
    }
    let (sess, a) = match attached {
        Some(x) => x,
        None => {
            let s = api.sessions.create(&user_id, &client, api.resume_timeout, protocol);
            match s.attach() {
                Ok(a) => (s, a),
                Err(e) => {
                    tracing::error!(error = %e, "could not attach a new session");
                    return;
                }
            }
        }
    };
    let mut rx = a.rx;
    let (mut tx, mut incoming) = socket.split();
    let ready = json!({ "op": "ready", "resumed": a.resumed, "sessionId": sess.id }).to_string();
    let sent = tokio::time::timeout(WRITE_TIMEOUT, tx.send(Message::Text(ready.into()))).await;
    if !matches!(sent, Ok(Ok(()))) {
        sess.detach(a.attach_id);
        return;
    }
    tracing::info!(session = %sess.id, client = %client, resumed = a.resumed, "client connected");
    if protocol == Protocol::V4 {
        sess.send_stats();
    }
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut answered = true;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(raw) = msg else { break };
                match tokio::time::timeout(WRITE_TIMEOUT, tx.send(Message::Text(raw.into()))).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
            frame = incoming.next() => match frame {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => answered = true,
            },
            _ = ping.tick() => {
                if !answered {
                    tracing::info!(session = %sess.id, "client stopped answering pings");
                    break;
                }
                answered = false;
                match tokio::time::timeout(WRITE_TIMEOUT, tx.send(Message::Ping(Default::default()))).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
        }
    }
    sess.detach(a.attach_id);
    let _ = tokio::time::timeout(Duration::from_secs(1), tx.send(Message::Close(None))).await;
    tracing::info!(session = %sess.id, client = %client, "client disconnected");
}
