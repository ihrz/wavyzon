//! Session and player routes, shared by `/v1` and `/v4` (port of Cadence's
//! `internal/api/players.go`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use wavyzon_player::{CrossfadeRequest, Filters, HostRequest, MAX_VOLUME, PlayerError};
use wavyzon_plugin::{LoadType, Track};
use wavyzon_voice::Server;

use super::tracks::decode;
use super::{Api, ApiError, ApiResult, LOAD_TIMEOUT, St, no_content, ok_json, parse};
use crate::session::{Guild, Session};

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrackField {
    #[serde(default)]
    encoded: Option<String>,
    /// Lavalink v4: load this identifier and play its first track.
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    user_data: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceField {
    #[serde(default)]
    token: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    bitrate: i64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlayerPatch {
    track: Option<TrackField>,
    position: Option<i64>,
    /// Accepted for Lavalink compatibility and ignored, as in Cadence.
    #[allow(dead_code)]
    end_time: Option<Value>,
    volume: Option<i64>,
    paused: Option<bool>,
    filters: Option<Filters>,
    voice: Option<VoiceField>,
    analyze: Option<bool>,
    bitrate: Option<i64>,
    next_track: Option<TrackField>,
}

fn bps(v: i64) -> i32 {
    v.clamp(0, i32::MAX as i64) as i32
}

impl Api {
    async fn track_of(&self, f: &TrackField) -> ApiResult<Option<Track>> {
        if let Some(e) = &f.encoded {
            return decode(e).map(Some);
        }
        let Some(id) = f.identifier.as_deref().filter(|s| !s.trim().is_empty()) else {
            return Ok(None);
        };
        let q = HashMap::from([("identifier".to_string(), id.to_string())]);
        let res = self.load(&q).await?;
        match res.kind {
            LoadType::Track | LoadType::Search if !res.tracks.is_empty() => Ok(res.tracks.into_iter().next()),
            LoadType::Error => Err(ApiError::bad_request(
                res.error.map(|f| f.to_string()).unwrap_or_default(),
            )),
            _ => Err(ApiError::bad_request(format!("no track found for identifier {id:?}"))),
        }
    }

    /// Apply a player update (port of Cadence's `applyPatch`).
    pub(super) async fn apply_patch(
        &self,
        sess: &Session,
        g: &Arc<Guild>,
        mut p: PlayerPatch,
        no_replace: bool,
    ) -> ApiResult<()> {
        if let Some(b) = p.bitrate {
            g.set_bitrate(bps(b));
        }
        if let Some(a) = p.analyze {
            g.player.set_analysis(a.then(|| sess.analysis_tap()));
        }
        if let Some(f) = p.filters.take() {
            g.player.set_filters(f)?;
        }
        if let Some(v) = p.volume {
            g.player.set_volume(v.clamp(0, MAX_VOLUME as i64) as i32);
        }
        if let Some(v) = p.voice.take() {
            let mut bitrate = bps(v.bitrate);
            if bitrate <= 0 {
                bitrate = g.preferred_bitrate();
            }
            if bitrate <= 0 {
                bitrate = self.bitrate;
            }
            let server = Server {
                guild_id: g.id.clone(),
                user_id: sess.user_id.clone(),
                channel_id: v.channel_id,
                session_id: v.session_id,
                token: v.token,
                endpoint: v.endpoint,
                bitrate,
            };
            g.update_voice(server)
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        if let Some(tf) = p.track.take() {
            // As in Lavalink, a `position` next to a track is that track's
            // start, never a seek, even when `noReplace` keeps the current
            // one. Cadence seeked: lavalink-client's queue advance
            // (`noReplace` + `position: 0`) then rewound the track a gapless
            // swap or crossfade had just started.
            let start = p.position.take().unwrap_or(0).max(0);
            match self.track_of(&tf).await? {
                None => g.player.stop(),
                Some(t) if no_replace && g.player.state().track.is_some() => {
                    // The track that just took over is acknowledged now
                    // rather than when the adoption window closes.
                    let encoded = t.encode();
                    if g.player.adopt(&t, start).is_some() {
                        g.set_user_data(&encoded, tf.user_data);
                    }
                }
                Some(t) => {
                    let paused = p.paused.take().unwrap_or(false);
                    g.set_user_data(&t.encode(), tf.user_data);
                    g.player.play(t, start, paused);
                }
            }
        }
        if let Some(next) = p.next_track.take() {
            self.apply_next(g, &next)?;
        }
        if let Some(paused) = p.paused {
            g.player.pause(paused);
        }
        if let Some(pos) = p.position {
            match g.player.seek(pos) {
                Ok(()) | Err(PlayerError::NothingToMix) => {}
                Err(e) => return Err(ApiError::conflict(e.to_string())),
            }
        }
        Ok(())
    }

    fn apply_next(&self, g: &Arc<Guild>, next: &TrackField) -> ApiResult<()> {
        let Some(enc) = &next.encoded else {
            g.player.clear_next();
            return Ok(());
        };
        let t = decode(enc)?;
        let g = g.clone();
        tokio::spawn(async move {
            match g.player.set_next(t.clone()).await {
                Ok(_) => {}
                Err(PlayerError::NextSuperseded) => {
                    tracing::debug!(guild = %g.id, track = %t.key(), "next track preload superseded")
                }
                Err(e) => {
                    tracing::warn!(guild = %g.id, track = %t.key(), error = %e, "next track could not be preloaded")
                }
            }
        });
        Ok(())
    }
}

fn no_replace(q: &HashMap<String, String>) -> bool {
    q.get("noReplace").is_some_and(|v| v == "true")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionPatch {
    resuming: Option<bool>,
    timeout: Option<i64>,
    wavyzon_events: Option<bool>,
    /// Cadence's name of `wavyzonEvents`.
    cadence_events: Option<bool>,
}

pub(super) async fn update_session(State(api): St, Path(session): Path<String>, body: Bytes) -> ApiResult {
    let sess = api.session(&session)?;
    let b: SessionPatch = parse(&body)?;
    if let Some(on) = b.wavyzon_events.or(b.cadence_events) {
        sess.set_extra_events(on);
    }
    let (mut resuming, mut timeout) = sess.settings();
    if let Some(r) = b.resuming {
        resuming = r;
    }
    if let Some(t) = b.timeout.filter(|t| *t > 0) {
        timeout = Duration::from_secs(t as u64);
    }
    sess.configure(resuming, timeout);
    Ok(ok_json(json!({ "resuming": resuming, "timeout": timeout.as_secs() })))
}

/// Players in the protocol of the URL (`/v1` or `/v4`).
pub(super) fn list(api: &Api, session: &str, v4: bool) -> ApiResult {
    let sess = api.session(session)?;
    let out: Vec<Value> = sess
        .guilds()
        .iter()
        .map(|g| {
            if v4 {
                serde_json::to_value(sess.player_v4(g)).unwrap_or_default()
            } else {
                sess.update(g)
            }
        })
        .collect();
    Ok(ok_json(out))
}

pub(super) async fn list_players(State(api): St, Path(session): Path<String>) -> ApiResult {
    list(&api, &session, false)
}

pub(super) async fn get_player(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let (sess, g) = api.guild(&session, &guild, false)?;
    Ok(ok_json(sess.update(&g)))
}

pub(super) async fn patch(
    api: &Api,
    session: &str,
    guild: &str,
    q: &HashMap<String, String>,
    body: &Bytes,
    v4: bool,
) -> ApiResult {
    let p: PlayerPatch = parse(body)?;
    let (sess, g) = api.guild(session, guild, true)?;
    api.apply_patch(&sess, &g, p, no_replace(q)).await?;
    Ok(ok_json(if v4 {
        serde_json::to_value(sess.player_v4(&g)).unwrap_or_default()
    } else {
        sess.update(&g)
    }))
}

pub(super) async fn update_player(
    State(api): St,
    Path((session, guild)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult {
    patch(&api, &session, &guild, &q, &body, false).await
}

pub(super) async fn delete_player(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let sess = api.session(&session)?;
    sess.destroy_guild(&guild).await;
    Ok(no_content())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeckBBody {
    #[serde(default)]
    encoded_track: String,
    #[serde(default)]
    position: i64,
}

pub(super) async fn load_deck_b(
    State(api): St,
    Path((session, guild)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    let (_, g) = api.guild(&session, &guild, false)?;
    let b: DeckBBody = parse(&body)?;
    let t = decode(&b.encoded_track)?;
    match tokio::time::timeout(LOAD_TIMEOUT, api.sources.playable(&t)).await {
        Ok(Ok(_)) => {}
        Ok(Err(f)) => return Err(ApiError::unprocessable(f.to_string())),
        Err(_) => return Err(ApiError::unprocessable("resolve timed out")),
    }
    let inst = g
        .player
        .load_deck_b(t, b.position)
        .await
        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    Ok(ok_json(json!({ "instance": inst })))
}

pub(super) async fn crossfade(
    State(api): St,
    Path((session, guild)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    let (_, g) = api.guild(&session, &guild, false)?;
    let req: CrossfadeRequest = parse(&body)?;
    match g.player.start_crossfade(req) {
        Ok(()) => Ok(no_content()),
        Err(e @ (PlayerError::NoDeckB | PlayerError::NothingToMix)) => Err(ApiError::conflict(e.to_string())),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

pub(super) async fn host(State(api): St, Path((session, guild)): Path<(String, String)>, body: Bytes) -> ApiResult {
    let (_, g) = api.guild(&session, &guild, false)?;
    let req: HostRequest = parse(&body)?;
    match g.player.start_host(req).await {
        Ok(()) => Ok(no_content()),
        Err(e) => {
            api.metrics.host.inc("refused");
            match e {
                PlayerError::NoDeckB | PlayerError::NothingToMix | PlayerError::TrackLoading => {
                    Err(ApiError::conflict(e.to_string()))
                }
                e => {
                    tracing::warn!(guild = %g.id, error = %e, "dj voice refused");
                    Err(ApiError::bad_request(e.to_string()))
                }
            }
        }
    }
}

pub(super) async fn analyze_deck(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let (sess, g) = api.guild(&session, &guild, false)?;
    g.player.set_analysis(Some(sess.analysis_tap()));
    Ok(no_content())
}

pub(super) async fn cancel_mix(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let (_, g) = api.guild(&session, &guild, false)?;
    g.player.cancel_pending();
    Ok(no_content())
}
