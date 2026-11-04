//! Track routes: `loadtracks`, decoding, prefetch and analysis.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use wavyzon_plugin::{Failure, LoadResult, LoadType, Track};

use super::{Api, ApiError, ApiResult, LOAD_TIMEOUT, MAX_PREFETCH, St, ok_json, parse, status_json};

/// `{encoded, info}` as on `/v1`.
pub fn encoded(t: &Track) -> Value {
    json!({ "encoded": t.encode(), "info": t })
}

/// A `/v1` load result.
pub fn encode_result(res: &LoadResult) -> Value {
    let tracks: Vec<Value> = res.tracks.iter().map(encoded).collect();
    match res.kind {
        LoadType::Track if !tracks.is_empty() => json!({ "loadType": "track", "data": tracks[0] }),
        LoadType::Playlist => json!({ "loadType": "playlist", "data": { "info": res.playlist, "tracks": tracks } }),
        LoadType::Search => json!({ "loadType": "search", "data": tracks }),
        LoadType::Error => json!({ "loadType": "error", "data": res.error }),
        _ => json!({ "loadType": "empty" }),
    }
}

pub fn decode(encoded: &str) -> ApiResult<Track> {
    Track::decode(encoded).map_err(|e| ApiError::bad_request(e.to_string()))
}

impl Api {
    /// Resolve an identifier with the `loadtracks` deadline.
    pub async fn load(&self, query: &HashMap<String, String>) -> ApiResult<LoadResult> {
        let id = query.get("identifier").map(|s| s.trim()).unwrap_or_default();
        if id.is_empty() {
            return Err(ApiError::bad_request("identifier is required"));
        }
        Ok(match tokio::time::timeout(LOAD_TIMEOUT, self.sources.load(id)).await {
            Ok(r) => r,
            Err(_) => LoadResult::error(Failure::common(
                "load",
                "load timed out",
                Some(format!("{}s", LOAD_TIMEOUT.as_secs())),
            )),
        })
    }

    pub fn decode_all(&self, body: &Bytes) -> ApiResult<Vec<Track>> {
        let raw: Vec<String> = parse(body)?;
        raw.iter().map(|e| decode(e)).collect()
    }
}

pub(super) async fn load_tracks(State(api): St, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    Ok(ok_json(encode_result(&api.load(&q).await?)))
}

pub(super) async fn decode_track(Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let t = decode(q.get("encodedTrack").map(String::as_str).unwrap_or_default())?;
    Ok(ok_json(encoded(&t)))
}

pub(super) async fn decode_tracks(State(api): St, body: Bytes) -> ApiResult {
    let tracks = api.decode_all(&body)?;
    Ok(ok_json(tracks.iter().map(encoded).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct PrefetchBody {
    #[serde(default)]
    tracks: Vec<String>,
}

pub(super) async fn prefetch(State(api): St, body: Bytes) -> ApiResult {
    let b: PrefetchBody = parse(&body)?;
    if b.tracks.is_empty() || b.tracks.len() > MAX_PREFETCH {
        return Err(ApiError::bad_request(format!(
            "tracks must hold 1 to {MAX_PREFETCH} encoded tracks"
        )));
    }
    let tracks = b.tracks.iter().map(|e| decode(e)).collect::<ApiResult<Vec<_>>>()?;
    let (mut queued, mut dropped) = (0, 0);
    for t in tracks {
        if api.sources.enqueue(t) {
            queued += 1;
        } else {
            dropped += 1;
        }
    }
    Ok(status_json(
        StatusCode::ACCEPTED,
        json!({ "queued": queued, "dropped": dropped }),
    ))
}

pub(super) async fn get_analysis(State(api): St, Path((source, identifier)): Path<(String, String)>) -> ApiResult {
    api.fingerprints
        .get(&source, &identifier)
        .map(|fp| ok_json(&*fp))
        .ok_or_else(|| ApiError::not_found("no fingerprint for this track yet"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeBody {
    #[serde(default)]
    encoded_track: String,
    #[serde(default)]
    force: bool,
}

pub(super) async fn analyze_track(State(api): St, body: Bytes) -> Response {
    let run = || -> ApiResult {
        let b: AnalyzeBody = parse(&body)?;
        let t = decode(&b.encoded_track)?;
        let queued = api.offline.enqueue(t, b.force);
        Ok(status_json(StatusCode::ACCEPTED, json!({ "queued": queued })))
    };
    run().unwrap_or_else(axum::response::IntoResponse::into_response)
}

#[cfg(test)]
mod tests {
    use wavyzon_plugin::Playlist;

    use super::*;

    #[test]
    fn v1_result_shapes() {
        assert_eq!(encode_result(&LoadResult::empty()), json!({ "loadType": "empty" }));
        let t = Track {
            source: "s".into(),
            identifier: "i".into(),
            ..Default::default()
        };
        let one = encode_result(&LoadResult::single(t.clone()));
        assert_eq!(one["data"]["info"]["source"], "s");
        assert_eq!(one["data"]["encoded"], t.encode());
        let p = encode_result(&LoadResult::playlist(
            Playlist {
                name: "P".into(),
                total: 3,
                ..Default::default()
            },
            vec![t],
        ));
        assert_eq!(p["data"]["info"], json!({ "name": "P", "total": 3 }));
        let e = encode_result(&LoadResult::error(Failure::common("s", "gone", None)));
        assert_eq!(
            e["data"],
            json!({ "severity": "common", "message": "gone", "source": "s" })
        );
    }
}
