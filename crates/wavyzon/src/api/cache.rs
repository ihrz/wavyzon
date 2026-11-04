//! Peer cache routes (port of Cadence's `internal/api/peerCache.go`).

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use tokio_util::io::ReaderStream;

use super::{ApiError, ApiResult, St, ok_json};

pub(super) async fn index(State(api): St) -> Response {
    ok_json(api.sources.audio_cache().map(|s| s.keys()).unwrap_or_default())
}

pub(super) async fn file(State(api): St, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let store = api
        .sources
        .audio_cache()
        .ok_or_else(|| ApiError::not_found("no audio cache on this node"))?;
    let key = q.get("key").map(String::as_str).unwrap_or_default();
    let (file, size) = store.get(key).ok_or_else(|| ApiError::not_found("not cached"))?;
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        body,
    )
        .into_response())
}
