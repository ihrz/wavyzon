//! The HTTP API (port of Cadence's `internal/api`): `/v1`, the Lavalink v4
//! layer, `/metrics`, the peer cache routes and plugin routes.

mod cache;
mod lyrics;
mod node;
mod players;
mod tracks;
mod v4;
mod ws;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use futures::FutureExt;
use serde::de::DeserializeOwned;
use serde_json::json;
use wavyzon_analysis::{Offline, Registry as Fingerprints};
use wavyzon_player::PlayerError;
use wavyzon_plugin::PluginManifest;
use wavyzon_sources::Sources;

use crate::metrics::NodeMetrics;
use crate::session::{Guild, Manager, Session, unix_ms};

/// The node version reported by `/v1/info`, `/version` and `/v4/info`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest accepted request body.
pub const MAX_BODY: usize = 1 << 20;
/// Deadline of `loadtracks` and deck B resolution.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// Most tracks one `prefetch` call accepts.
pub const MAX_PREFETCH: usize = 10;
/// Built-in filter keys.
pub const FILTERS: &[&str] = &[
    "equalizer",
    "timescale",
    "tremolo",
    "vibrato",
    "rotation",
    "lowPass",
    "karaoke",
    "reverb",
];

/// Shared state of every handler.
pub struct Api {
    pub password: String,
    pub cache_secret: String,
    pub sessions: Arc<Manager>,
    pub sources: Sources,
    pub fingerprints: Arc<Fingerprints>,
    pub offline: Arc<Offline>,
    pub resume_timeout: Duration,
    pub bitrate: i32,
    pub started: Instant,
    pub started_unix_ms: i64,
    pub plugins: Vec<PluginManifest>,
    pub plugin_filters: Vec<String>,
    pub metrics: Arc<NodeMetrics>,
}

type St = State<Arc<Api>>;

/// A handler error, rendered by [`finish`] in the Cadence/Lavalink error shape
/// (`timestamp`, `status`, `error`, `message`, `path`).
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

#[derive(Clone)]
struct ErrorMark(String);

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, m)
    }
    pub fn unprocessable(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, m)
    }
}

impl From<PlayerError> for ApiError {
    fn from(e: PlayerError) -> Self {
        let status = match &e {
            PlayerError::NoDeckB
            | PlayerError::NothingToMix
            | PlayerError::NothingPlaying
            | PlayerError::NextSuperseded
            | PlayerError::TrackLoading
            | PlayerError::Destroyed => StatusCode::CONFLICT,
            PlayerError::Resolve(_) => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_REQUEST,
        };
        Self::new(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = self.status.into_response();
        r.extensions_mut().insert(ErrorMark(self.message));
        r
    }
}

pub type ApiResult<T = Response> = Result<T, ApiError>;

fn error_body(status: StatusCode, message: &str, path: &str) -> Response {
    let body = json!({
        "timestamp": unix_ms(),
        "status": status.as_u16(),
        "error": status.canonical_reason().unwrap_or(""),
        "message": message,
        "path": path,
    });
    (status, axum::Json(body)).into_response()
}

/// Outermost layer: renders [`ApiError`]s (and any other non-JSON error,
/// such as a 405 or a body over the limit) in the error shape, and turns
/// handler panics into `500`s.
async fn finish(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let res = match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(r) => r,
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!(path, panic = %what, "handler panicked");
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal error", &path);
        }
    };
    if let Some(ErrorMark(m)) = res.extensions().get::<ErrorMark>().cloned() {
        return error_body(res.status(), &m, &path);
    }
    let status = res.status();
    let is_json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if (status.is_client_error() || status.is_server_error()) && !is_json && !path.starts_with("/v1/plugins/") {
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let msg = if text.trim().is_empty() {
            status.canonical_reason().unwrap_or("").to_string()
        } else {
            text.trim().to_string()
        };
        return error_body(status, &msg, &path);
    }
    res
}

/// Constant-time comparison (the length still leaks, as with Go's
/// `subtle.ConstantTimeCompare`).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Every route needs the password; the peer cache routes also accept
/// `WAVYZON_CACHE_SECRET`.
async fn auth(State(api): St, req: Request, next: Next) -> Response {
    let got = req
        .headers()
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    let mut ok = constant_time_eq(got, api.password.as_bytes());
    if !ok && !api.cache_secret.is_empty() && req.uri().path().starts_with("/v1/cache/") {
        ok = constant_time_eq(got, api.cache_secret.as_bytes());
    }
    if !ok {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing or wrong Authorization header").into_response();
    }
    next.run(req).await
}

/// Parse a JSON body.
pub fn parse<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(e.to_string()))
}

pub fn ok_json(v: impl serde::Serialize) -> Response {
    axum::Json(v).into_response()
}

pub fn status_json(status: StatusCode, v: impl serde::Serialize) -> Response {
    (status, axum::Json(v)).into_response()
}

impl Api {
    pub fn session(&self, id: &str) -> ApiResult<Arc<Session>> {
        self.sessions
            .get(id)
            .ok_or_else(|| ApiError::not_found("unknown session"))
    }

    pub fn guild(&self, session: &str, guild: &str, create: bool) -> ApiResult<(Arc<Session>, Arc<Guild>)> {
        let s = self.session(session)?;
        let g = s
            .guild(guild, create)
            .ok_or_else(|| ApiError::not_found("no player for this guild"))?;
        Ok((s, g))
    }
}

/// The whole HTTP surface. `plugin_routes` are mounted at
/// `/v1/plugins/<name>`; everything is behind [`auth`].
pub fn router(api: Arc<Api>, plugin_routes: Vec<(String, Router)>) -> Router {
    let mix = |r: Router<Arc<Api>>, base: &str| {
        r.route(&format!("{base}/mix/deckb"), post(players::load_deck_b))
            .route(&format!("{base}/mix/crossfade"), post(players::crossfade))
            .route(&format!("{base}/mix/host"), post(players::host))
            .route(&format!("{base}/mix/analyze"), post(players::analyze_deck))
            .route(&format!("{base}/mix"), axum::routing::delete(players::cancel_mix))
    };
    let mut r: Router<Arc<Api>> = Router::new()
        .route("/v1/websocket", get(ws::websocket_v1))
        .route("/v1/info", get(node::info))
        .route("/v1/stats", get(node::stats))
        .route("/v1/clock", get(node::clock))
        .route("/v1/plugins", get(node::plugins))
        .route("/metrics", get(node::metrics))
        .route("/v1/loadtracks", get(tracks::load_tracks))
        .route("/v1/decodetrack", get(tracks::decode_track))
        .route("/v1/decodetracks", post(tracks::decode_tracks))
        .route("/v1/prefetch", post(tracks::prefetch))
        .route("/v1/mix/analysis/{source}/{*identifier}", get(tracks::get_analysis))
        .route("/v1/mix/analyze", post(tracks::analyze_track))
        .route("/v1/sessions/{session}", patch(players::update_session))
        .route("/v1/sessions/{session}/players", get(players::list_players))
        .route(
            "/v1/sessions/{session}/players/{guild}",
            get(players::get_player)
                .patch(players::update_player)
                .delete(players::delete_player),
        )
        .route("/v1/lyrics", get(lyrics::by_track))
        .route(
            "/v1/sessions/{session}/players/{guild}/track/lyrics",
            get(lyrics::current),
        )
        .route("/v1/cache/index", get(cache::index))
        .route("/v1/cache/file", get(cache::file));
    r = mix(r, "/v1/sessions/{session}/players/{guild}");
    r = v4::routes(r);
    r = mix(r, "/v4/sessions/{session}/players/{guild}");
    let mut app: Router = r
        .fallback(|| async { ApiError::not_found("no such route") })
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(api.clone());
    for (name, plugin) in plugin_routes {
        let base = format!("/v1/plugins/{name}");
        app = app.nest(&base, plugin);
    }
    app.layer(middleware::from_fn_with_state(api, auth))
        .layer(middleware::from_fn(finish))
}

/// An empty `204 No Content`.
pub fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// A body from bytes with a content type.
pub fn typed(content_type: &'static str, body: impl Into<Body>) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body.into()).into_response()
}
