//! Lyrics, in the shape of Lavalink's LavaLyrics plugin API so Lavalink
//! clients can read them (not in Cadence). Served by
//! [`wavyzon_sources::Sources::lyrics`], in `WAVYZON_LYRICS_ORDER` (default
//! LRCLIB, lyrics.ovh, Genius). LRCLIB's synced lyrics fill `lines`.
//!
//! `200` with the lyrics, `204` when there are none, `404` when lyrics are
//! disabled or nothing is playing, `502` when every provider failed.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::json;
use wavyzon_plugin::Track;

use super::tracks::decode;
use super::{Api, ApiError, ApiResult, St, no_content, ok_json};

/// Name advertised in `/v4/info` `plugins`, which Lavalink clients look for
/// before calling the lyrics routes.
pub const PLUGIN_NAME: &str = "lavalyrics-plugin";

/// `GET /v{1,4}/lyrics?track=<encoded>` (`encodedTrack` is accepted too;
/// LavaLyrics' `skipTrackSource` is ignored: no provider reads the track's source).
pub(super) async fn by_track(State(api): St, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let encoded = q
        .get("track")
        .or_else(|| q.get("encodedTrack"))
        .map(String::as_str)
        .unwrap_or_default();
    if encoded.is_empty() {
        return Err(ApiError::bad_request("missing track"));
    }
    lyrics_of(&api, &decode(encoded)?).await
}

/// `GET /v{1,4}/sessions/{session}/players/{guild}/track/lyrics`: the
/// lyrics of the track the player is on.
pub(super) async fn current(State(api): St, Path((session, guild)): Path<(String, String)>) -> ApiResult {
    let (_, g) = api.guild(&session, &guild, false)?;
    let Some(track) = g.player.state().track else {
        return Err(ApiError::not_found("nothing is playing"));
    };
    lyrics_of(&api, &track).await
}

async fn lyrics_of(api: &Api, track: &Track) -> ApiResult {
    if !api.sources.lyrics_enabled() {
        return Err(ApiError::not_found("lyrics are disabled on this node"));
    }
    match api.sources.lyrics(track).await {
        Ok(Some(l)) => Ok(ok_json(json!({
            "sourceName": track.source,
            "provider": l.provider,
            "text": &*l.text,
            "lines": l.lines.iter().map(|line| json!({
                "timestamp": line.timestamp_ms,
                "duration": line.duration_ms,
                "line": &*line.line,
                "plugin": {},
            })).collect::<Vec<_>>(),
            "plugin": {},
        }))),
        Ok(None) => Ok(no_content()),
        Err(f) => Err(ApiError::new(StatusCode::BAD_GATEWAY, f.to_string())),
    }
}
