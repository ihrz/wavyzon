//! The Lavalink v4 layer (port of Cadence's `internal/api/lavalinkV4.go`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use serde_json::json;
use wavyzon_analysis::ANALYSIS_VERSION;

use super::tracks::{analyze_track, decode, get_analysis, prefetch};
use super::{Api, ApiResult, FILTERS, St, VERSION, ok_json, players, typed, ws};
use crate::lavalink::{self, Cpu, Memory, Stats, parse_version};
use crate::metrics::resident_bytes;

/// Advertised so clients that insist on YouTube still search.
const ALIAS_SOURCE: &str = "youtube";

pub(super) fn routes(r: Router<Arc<Api>>) -> Router<Arc<Api>> {
    r.route("/v4/websocket", get(ws::websocket_v4))
        .route("/version", get(version))
        .route("/v4/info", get(info))
        .route("/v4/stats", get(stats))
        .route("/v4/loadtracks", get(load_tracks))
        .route("/v4/decodetrack", get(decode_track))
        .route("/v4/decodetracks", post(decode_tracks))
        .route("/v4/prefetch", post(prefetch))
        .route("/v4/sessions/{session}", patch(players::update_session))
        .route("/v4/sessions/{session}/players", get(list_players))
        .route(
            "/v4/sessions/{session}/players/{guild}",
            get(get_player).patch(update_player).delete(players::delete_player),
        )
        .route("/v4/sessions/{session}/mix/analyzetrack", post(analyze_track_v4))
        .route("/v4/mix/analysis/{source}/{*identifier}", get(get_analysis))
        .route("/v4/lyrics", get(super::lyrics::by_track))
        .route(
            "/v4/sessions/{session}/players/{guild}/track/lyrics",
            get(super::lyrics::current),
        )
}

async fn version() -> axum::response::Response {
    typed("text/plain", VERSION)
}

async fn info(State(api): St) -> axum::response::Response {
    let mut managers = api.sources.names();
    if !managers.iter().any(|m| m == ALIAS_SOURCE) {
        managers.push(ALIAS_SOURCE.into());
    }
    let mut plugins: Vec<_> = api
        .plugins
        .iter()
        .map(|p| json!({ "name": p.name, "version": p.version }))
        .collect();
    // The lyrics routes speak LavaLyrics' API, which clients detect by name.
    if api.sources.lyrics().is_some() {
        plugins.push(json!({ "name": super::lyrics::PLUGIN_NAME, "version": VERSION }));
    }
    ok_json(json!({
        "version": parse_version(VERSION),
        "buildTime": api.started_unix_ms,
        "git": { "branch": "main", "commit": "", "commitTime": 0 },
        "jvm": env!("WAVYZON_RUSTC_VERSION"),
        "lavaplayer": format!("wavyzon {VERSION}"),
        "sourceManagers": managers,
        "filters": FILTERS,
        "plugins": plugins,
        "wavyzon": VERSION,
        "cadence": VERSION,
        "wireClock": true,
        "mixing": true,
        "dave": wavyzon_voice::dave_available(),
        "analysis": ANALYSIS_VERSION,
    }))
}

impl Api {
    /// Lavalink `stats` (REST and the WebSocket `stats` op).
    pub fn stats_v4(&self) -> Stats {
        let (players, playing) = self.sessions.counts();
        let rss = resident_bytes();
        Stats {
            players,
            playing_players: playing,
            uptime: self.started.elapsed().as_millis() as i64,
            memory: Memory {
                free: 0,
                used: rss,
                allocated: rss,
                reservable: rss,
            },
            cpu: Cpu {
                cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
                system_load: 0.0,
                lavalink_load: 0.0,
            },
        }
    }
}

async fn stats(State(api): St) -> axum::response::Response {
    ok_json(api.stats_v4())
}

async fn load_tracks(State(api): St, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    Ok(ok_json(lavalink::load_result(&api.load(&q).await?)))
}

async fn decode_track(Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let t = decode(q.get("encodedTrack").map(String::as_str).unwrap_or_default())?;
    Ok(ok_json(lavalink::from_track(&t)))
}

async fn decode_tracks(State(api): St, body: Bytes) -> ApiResult {
    let tracks = api.decode_all(&body)?;
    Ok(ok_json(tracks.iter().map(lavalink::from_track).collect::<Vec<_>>()))
}

async fn list_players(State(api): St, Path(session): Path<String>) -> ApiResult {
    players::list(&api, &session, true)
}

async fn get_player(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let (sess, g) = api.guild(&session, &guild, false)?;
    Ok(ok_json(sess.player_v4(&g)))
}

async fn update_player(
    State(api): St,
    Path((session, guild)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult {
    players::patch(&api, &session, &guild, &q, &body, true).await
}

async fn analyze_track_v4(state: St, Path(_session): Path<String>, body: Bytes) -> axum::response::Response {
    analyze_track(state, body).await
}
