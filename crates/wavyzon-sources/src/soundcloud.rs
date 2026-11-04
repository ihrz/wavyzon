//! SoundCloud: search, track/set links, short links and playback
//! (progressive MP3, or HLS MP3 segments joined into one stream).
//!
//! Without a configured client id, one is scraped from the web player's
//! scripts, cached for 6 h and re-scraped when the API rejects it; a failed
//! scrape pauses scraping for a minute.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use regex::Regex;
use serde::Deserialize;
use tokio_util::io::StreamReader;
use wavyzon_plugin::{Failure, Format, LoadResult, Playable, Playlist, Source, Stream, Track};

use crate::cache::Lru;
use crate::httpx::{HttpClient, Req};
use crate::matching::normalize_isrc;
use crate::util::{bad_json, query, response_body};

pub const NAME: &str = "soundcloud";
pub const SEARCH_PREFIX: &str = "scsearch";
const SEARCH_LIMIT: usize = 10;
const ID_BATCH: usize = 50;
const PLAYLIST_LIMIT: usize = 500;
const CLIENT_ID_TTL: Duration = Duration::from_secs(6 * 3600);
const SCRAPE_COOLDOWN: Duration = Duration::from_secs(60);
const TRACK_CACHE_TTL: Duration = Duration::from_secs(600);
const TRACK_ENTRIES: usize = 20_000;
const MAX_SEGMENTS: usize = 2_000;
const MAX_PLAYLIST: usize = 1 << 20;

/// API and web base URLs (injectable for tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub api: String,
    pub web: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api-v2.soundcloud.com".into(),
            web: "https://soundcloud.com".into(),
        }
    }
}

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^https?://(?:www\.|m\.)?soundcloud\.com/[^/?#]+/[^?#]+").unwrap())
}
fn short_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^https?://on\.soundcloud\.com/").unwrap())
}
fn script_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"<script[^>]+src="(https?://[^"]+/assets/[^"]+\.js)""#).unwrap())
}
fn client_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"client_id\s*[:=]\s*"([A-Za-z0-9]{32})""#).unwrap())
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TcFormat {
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    mime_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Transcoding {
    #[serde(default)]
    url: String,
    #[serde(default)]
    snipped: bool,
    #[serde(default)]
    format: TcFormat,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct User {
    #[serde(default)]
    username: String,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Publisher {
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Media {
    #[serde(default)]
    transcodings: Vec<Transcoding>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiTrack {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    permalink_url: String,
    #[serde(default)]
    artwork_url: Option<String>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    streamable: Option<bool>,
    #[serde(default)]
    user: User,
    #[serde(default)]
    publisher_metadata: Option<Publisher>,
    #[serde(default)]
    media: Media,
    /// Set on playlists returned by `/resolve`.
    #[serde(default)]
    tracks: Vec<ApiTrack>,
}

impl ApiTrack {
    fn title(&self) -> &str {
        self.title.as_deref().unwrap_or("")
    }

    fn playable(&self) -> bool {
        let policy = self.policy.as_deref().unwrap_or("");
        if policy == "SNIP" || policy == "BLOCK" || self.streamable == Some(false) {
            return false;
        }
        self.best_transcoding().is_some()
    }

    fn best_transcoding(&self) -> Option<&Transcoding> {
        let mut hls = None;
        for tc in &self.media.transcodings {
            if tc.snipped || !tc.format.mime_type.starts_with("audio/mpeg") {
                continue;
            }
            match tc.format.protocol.as_str() {
                "progressive" => return Some(tc),
                "hls" if hls.is_none() => hls = Some(tc),
                _ => {}
            }
        }
        hls
    }

    fn to_track(&self) -> Track {
        let mut author = self.user.username.clone();
        let mut isrc = String::new();
        if let Some(p) = &self.publisher_metadata {
            if let Some(a) = p.artist.as_deref().filter(|a| !a.is_empty()) {
                author = a.to_string();
            }
            isrc = p.isrc.clone().unwrap_or_default();
        }
        let art = self
            .artwork_url
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| self.user.avatar_url.clone())
            .unwrap_or_default();
        Track {
            source: NAME.into(),
            identifier: self.id.to_string(),
            title: self.title().to_string(),
            author,
            duration_ms: self.duration,
            isrc: normalize_isrc(&isrc),
            artwork_url: art.replacen("-large.", "-t500x500.", 1),
            uri: self.permalink_url.clone(),
            ..Default::default()
        }
    }
}

#[derive(Default)]
struct IdState {
    client_id: String,
    expires: Option<Instant>,
    failed_until: Option<Instant>,
    last_failure: Option<Failure>,
}

/// The SoundCloud source.
pub struct SoundCloud {
    http: Arc<HttpClient>,
    ep: Endpoints,
    fixed_id: Option<String>,
    ids: tokio::sync::Mutex<IdState>,
    tracks: Lru<String, ApiTrack>,
}

impl SoundCloud {
    pub fn new(http: Arc<HttpClient>, client_id: Option<String>, ep: Endpoints) -> Self {
        Self {
            http,
            ep,
            fixed_id: client_id.filter(|c| !c.is_empty()),
            ids: tokio::sync::Mutex::new(IdState::default()),
            tracks: Lru::new(TRACK_ENTRIES, TRACK_CACHE_TTL),
        }
    }

    async fn id(&self, rejected: &str) -> Result<String, Failure> {
        if let Some(id) = &self.fixed_id {
            return Ok(id.clone());
        }
        let mut st = self.ids.lock().await;
        let now = Instant::now();
        if !st.client_id.is_empty() && st.client_id != rejected && st.expires.is_some_and(|e| now < e) {
            return Ok(st.client_id.clone());
        }
        if st.failed_until.is_some_and(|f| now < f) {
            if let Some(f) = &st.last_failure {
                return Err(f.clone());
            }
        }
        match self.scrape().await {
            Ok(id) => {
                st.client_id = id.clone();
                st.expires = Some(Instant::now() + CLIENT_ID_TTL);
                st.failed_until = None;
                Ok(id)
            }
            Err(f) => {
                tracing::warn!(source = NAME, cooldown_seconds = SCRAPE_COOLDOWN.as_secs(), error = %f, "could not find a client id, pausing scrapes");
                st.failed_until = Some(Instant::now() + SCRAPE_COOLDOWN);
                st.last_failure = Some(f.clone());
                Err(f)
            }
        }
    }

    async fn scrape(&self) -> Result<String, Failure> {
        let page = self
            .http
            .get(&format!("{}/", self.ep.web), &[])
            .await
            .map_err(|e| e.to_failure(NAME))?;
        let page = String::from_utf8_lossy(&page);
        let scripts: Vec<String> = script_re().captures_iter(&page).map(|c| c[1].to_string()).collect();
        for script in scripts.iter().rev() {
            match self.http.get(script, &[]).await {
                Ok(js) => {
                    if let Some(m) = client_id_re().captures(&String::from_utf8_lossy(&js)) {
                        return Ok(m[1].to_string());
                    }
                }
                Err(e) => tracing::debug!(source = NAME, script = %script, error = %e, "web player script unavailable"),
            }
        }
        Err(Failure::suspicious(
            NAME,
            "client id unavailable",
            Some("could not find a soundcloud client id in the web player".into()),
        ))
    }

    async fn api<T: serde::de::DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T, Failure> {
        let mut rejected = String::new();
        for attempt in 0..2 {
            let id = self.id(&rejected).await?;
            rejected = id.clone();
            let mut pairs: Vec<(&str, &str)> = params.iter().filter(|(k, _)| *k != "client_id").copied().collect();
            pairs.push(("client_id", &id));
            let url = format!("{}{}?{}", self.ep.api, path, query(&pairs));
            match self.http.get(&url, &[]).await {
                Ok(body) => return serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e)),
                Err(e) => {
                    if attempt == 0 && self.fixed_id.is_none() && matches!(e.status(), Some(401 | 403)) {
                        continue;
                    }
                    return Err(e.to_failure(NAME));
                }
            }
        }
        Err(Failure::suspicious(NAME, "client id rejected twice", None))
    }

    fn keep(&self, input: Vec<ApiTrack>) -> Vec<Track> {
        let mut out = Vec::with_capacity(input.len());
        for t in input {
            if (!t.kind.is_empty() && t.kind != "track") || !t.playable() {
                continue;
            }
            out.push(t.to_track());
            self.tracks.set(t.id.to_string(), t);
        }
        out
    }

    async fn api_track(&self, id: &str) -> Result<ApiTrack, Failure> {
        if let Some(t) = self.tracks.get(&id.to_string()) {
            return Ok(t);
        }
        let t: ApiTrack = self.api(&format!("/tracks/{}", crate::util::esc(id)), &[]).await?;
        self.tracks.set(id.to_string(), t.clone());
        Ok(t)
    }

    async fn fill(&self, mut input: Vec<ApiTrack>) -> Result<Vec<ApiTrack>, Failure> {
        input.truncate(PLAYLIST_LIMIT);
        let missing: Vec<String> = input
            .iter()
            .filter(|t| t.title.is_none())
            .map(|t| t.id.to_string())
            .collect();
        let mut got = std::collections::HashMap::new();
        for chunk in missing.chunks(ID_BATCH) {
            let ids = chunk.join(",");
            let batch: Vec<Option<ApiTrack>> = self.api("/tracks", &[("ids", &ids)]).await?;
            for t in batch.into_iter().flatten() {
                got.insert(t.id, t);
            }
        }
        let mut out = Vec::with_capacity(input.len());
        for mut t in input {
            if t.title.is_none() {
                match got.remove(&t.id) {
                    Some(full) => t = full,
                    None => continue,
                }
            }
            t.kind = "track".into();
            out.push(t);
        }
        Ok(out)
    }

    async fn stream_url(&self, id: &str) -> Result<(String, String), Failure> {
        let t = self.api_track(id).await?;
        let tc = match t.best_transcoding() {
            Some(tc) if t.playable() => tc.clone(),
            _ => {
                return Err(Failure::common(
                    NAME,
                    "stream unavailable",
                    Some("track has no full-length mp3 stream".into()),
                ));
            }
        };
        let u =
            url::Url::parse(&tc.url).map_err(|e| Failure::common(NAME, "bad transcoding url", Some(e.to_string())))?;
        let params: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        #[derive(Deserialize)]
        struct Out {
            #[serde(default)]
            url: String,
        }
        let out: Out = self.api(u.path(), &params).await?;
        if out.url.is_empty() {
            return Err(Failure::common(NAME, "no stream url returned", None));
        }
        Ok((out.url, tc.format.protocol))
    }

    async fn hls_segments(&self, playlist_url: &str) -> Result<Vec<String>, Failure> {
        let body = self.http.get(playlist_url, &[]).await.map_err(|e| e.to_failure(NAME))?;
        if body.len() > MAX_PLAYLIST {
            return Err(Failure::common(NAME, "hls playlist too large", None));
        }
        let base = url::Url::parse(playlist_url)
            .map_err(|e| Failure::common(NAME, "bad playlist url", Some(e.to_string())))?;
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(&body).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let seg = base
                .join(line)
                .map_err(|e| Failure::common(NAME, "bad segment url", Some(e.to_string())))?;
            out.push(seg.to_string());
            if out.len() > MAX_SEGMENTS {
                return Err(Failure::common(NAME, "hls playlist has too many segments", None));
            }
        }
        if out.is_empty() {
            return Err(Failure::common(NAME, "hls playlist has no segments", None));
        }
        Ok(out)
    }
}

#[async_trait]
impl Source for SoundCloud {
    fn name(&self) -> &str {
        NAME
    }

    fn search_prefix(&self) -> Option<&str> {
        Some(SEARCH_PREFIX)
    }

    async fn search(&self, q: &str) -> Result<Vec<Track>, Failure> {
        #[derive(Deserialize)]
        struct Res {
            #[serde(default)]
            collection: Vec<ApiTrack>,
        }
        let limit = SEARCH_LIMIT.to_string();
        let res: Res = self.api("/search/tracks", &[("q", q), ("limit", &limit)]).await?;
        Ok(self.keep(res.collection))
    }

    fn matches_url(&self, url: &str) -> bool {
        link_re().is_match(url) || short_re().is_match(url)
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let clean = url.split('?').next().unwrap_or(url);
        let res: ApiTrack = self.api("/resolve", &[("url", clean)]).await?;
        match res.kind.as_str() {
            "track" => match self.keep(vec![res]).into_iter().next() {
                Some(t) => Ok(LoadResult::single(t)),
                None => Ok(LoadResult::error(Failure::common(
                    NAME,
                    "this track is a preview or cannot be streamed",
                    None,
                ))),
            },
            "playlist" => {
                let total = res.tracks.len();
                let playlist = Playlist {
                    name: res.title().to_string(),
                    artwork_url: res.artwork_url.clone().unwrap_or_default(),
                    uri: res.permalink_url.clone(),
                    total,
                };
                let full = self.fill(res.tracks).await?;
                let tracks = self.keep(full);
                if tracks.is_empty() {
                    return Ok(LoadResult::empty());
                }
                Ok(LoadResult::playlist(playlist, tracks))
            }
            _ => Ok(LoadResult::empty()),
        }
    }

    async fn track(&self, id: &str) -> Result<Track, Failure> {
        Ok(self.api_track(id).await?.to_track())
    }

    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for SoundCloud {
    async fn open(&self, track: &Track, _start_ms: i64) -> Result<Stream, Failure> {
        let (url, protocol) = self.stream_url(&track.identifier).await?;
        if protocol == "progressive" {
            let req = Req::get(&url).map_err(|e| e.to_failure(NAME))?;
            let res = self.http.stream(&req).await.map_err(|e| e.to_failure(NAME))?;
            return Ok(Stream {
                body: response_body(res),
                format: Format::mp3(),
                start_sample: 0,
                live: false,
            });
        }
        let segments = self.hls_segments(&url).await?;
        let total = segments.len();
        let http = self.http.clone();
        let parts = futures::stream::iter(segments.into_iter().enumerate())
            .then(move |(i, seg)| {
                let http = http.clone();
                async move {
                    let req = Req::get(&seg).map_err(std::io::Error::other)?;
                    http.stream(&req)
                        .await
                        .map_err(|e| std::io::Error::other(format!("hls segment {} of {total}: {e}", i + 1)))
                }
            })
            .map_ok(|res| res.bytes_stream().map_err(std::io::Error::other))
            .try_flatten();
        Ok(Stream {
            body: Box::new(StreamReader::new(Box::pin(parts))),
            format: Format::mp3(),
            start_sample: 0,
            live: false,
        })
    }

    async fn warm(&self, track: &Track) {
        let _ = self.api_track(&track.identifier).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_with;
    use axum::Router;
    use axum::extract::{Path, Query};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::io::AsyncReadExt;

    const TEST_CLIENT_ID: &str = "abcdefghijklmnopqrstuvwxyz123456";
    const AUDIO: &str = "PROGRESSIVE-MP3-BYTES";
    const SEG0: &str = "HLS-SEGMENT-ONE|";
    const SEG1: &str = "HLS-SEGMENT-TWO";

    #[derive(Default)]
    struct Fake {
        reject_first: AtomicBool,
        web_down: AtomicBool,
        scrapes: AtomicU32,
    }

    fn track(id: i64, title: &str, policy: &str, tcs: Vec<Value>) -> Value {
        json!({
            "id": id, "kind": "track", "title": title, "duration": 200000, "policy": policy,
            "permalink_url": format!("https://soundcloud.com/artist/t{id}"),
            "artwork_url": "https://i1.sndcdn.com/artworks-x-large.jpg",
            "user": {"username": "uploader"},
            "publisher_metadata": {"artist": "Real Artist", "isrc": "us-abc-12-34567"},
            "media": {"transcodings": tcs},
        })
    }

    fn tc(base: &str, protocol: &str, path: &str, snipped: bool) -> Value {
        json!({"url": format!("{base}{path}"), "snipped": snipped, "format": {"protocol": protocol, "mime_type": "audio/mpeg"}})
    }

    type Q = Query<HashMap<String, String>>;

    async fn fake() -> (Arc<Fake>, SoundCloud) {
        let f = Arc::new(Fake::default());
        let st = f.clone();
        let base = serve_with(move |base: String| {
            let progressive = track(1, "Song One", "ALLOW", vec![tc(&base, "hls", "/media/1/hls", false), tc(&base, "progressive", "/media/1/progressive", false)]);
            let hls_only = track(2, "Song Two", "MONETIZE", vec![tc(&base, "progressive", "/media/2/snip", true), tc(&base, "hls", "/media/2/hls", false)]);
            let preview = track(3, "Preview Only", "SNIP", vec![tc(&base, "progressive", "/media/3/progressive", false)]);
            let by_id: Arc<HashMap<String, Value>> =
                Arc::new([("1", &progressive), ("2", &hls_only), ("3", &preview)].into_iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
            let search = json!({"collection": [preview, progressive, hls_only]});
            let guard = {
                let st = st.clone();
                move |q: &HashMap<String, String>| -> Option<Response> {
                    if q.get("client_id").map(String::as_str) != Some(TEST_CLIENT_ID) {
                        return Some((StatusCode::UNAUTHORIZED, "bad client").into_response());
                    }
                    if st.reject_first.swap(false, Ordering::SeqCst) {
                        return Some((StatusCode::UNAUTHORIZED, "rotated").into_response());
                    }
                    None
                }
            };
            let g1 = guard.clone();
            let g2 = guard.clone();
            let g3 = guard.clone();
            let g4 = guard.clone();
            let g5 = guard.clone();
            let g6 = guard.clone();
            let b1 = by_id.clone();
            let b2 = by_id.clone();
            let prog = progressive.clone();
            let (base1, base2, base3, base4) = (base.clone(), base.clone(), base.clone(), base.clone());
            let st_web = st.clone();
            Router::new()
                .route(
                    "/",
                    get(move || {
                        let st = st_web.clone();
                        let base = base1.clone();
                        async move {
                            st.scrapes.fetch_add(1, Ordering::SeqCst);
                            if st.web_down.load(Ordering::SeqCst) {
                                return (StatusCode::BAD_GATEWAY, "down".to_string()).into_response();
                            }
                            format!(r#"<html><script crossorigin src="{base}/assets/0-vendor.js"></script><script crossorigin src="{base}/assets/49-app.js"></script></html>"#).into_response()
                        }
                    }),
                )
                .route("/assets/0-vendor.js", get(|| async { "var nothing=1;" }))
                .route("/assets/49-app.js", get(|| async { format!(r#"e.exports={{client_id:"{TEST_CLIENT_ID}",x:1}}"#) }))
                .route(
                    "/search/tracks",
                    get(move |Query(q): Q| {
                        let r = g1(&q);
                        let s = search.clone();
                        async move { r.unwrap_or_else(|| axum::Json(s).into_response()) }
                    }),
                )
                .route(
                    "/tracks/{id}",
                    get(move |Path(id): Path<String>, Query(q): Q| {
                        let r = g2(&q);
                        let t = b1.get(&id).cloned().unwrap_or(Value::Null);
                        async move { r.unwrap_or_else(|| axum::Json(t).into_response()) }
                    }),
                )
                .route(
                    "/tracks",
                    get(move |Query(q): Q| {
                        let r = g3(&q);
                        let out: Vec<Value> = q.get("ids").map(|ids| ids.split(',').map(|id| b2.get(id).cloned().unwrap_or(Value::Null)).collect()).unwrap_or_default();
                        async move { r.unwrap_or_else(|| axum::Json(out).into_response()) }
                    }),
                )
                .route(
                    "/resolve",
                    get(move |Query(q): Q| {
                        let r = g4(&q);
                        let body = if q.get("url").is_some_and(|u| u.contains("/sets/")) {
                            json!({"kind": "playlist", "title": "My Set", "permalink_url": "https://soundcloud.com/a/sets/s", "tracks": [prog, {"id": 2}, {"id": 3}]})
                        } else {
                            prog.clone()
                        };
                        async move { r.unwrap_or_else(|| axum::Json(body).into_response()) }
                    }),
                )
                .route(
                    "/media/1/progressive",
                    get(move |Query(q): Q| {
                        let r = g5(&q);
                        let u = format!("{base2}/cf/audio.mp3");
                        async move { r.unwrap_or_else(|| axum::Json(json!({ "url": u })).into_response()) }
                    }),
                )
                .route(
                    "/media/2/hls",
                    get(move |Query(q): Q| {
                        let r = g6(&q);
                        let u = format!("{base3}/cf/list/playlist.m3u8");
                        async move { r.unwrap_or_else(|| axum::Json(json!({ "url": u })).into_response()) }
                    }),
                )
                .route("/cf/audio.mp3", get(|| async { AUDIO }))
                .route(
                    "/cf/list/playlist.m3u8",
                    get(move || {
                        let b = base4.clone();
                        async move { format!("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg0.mp3\n#EXTINF:10.0,\n{b}/cf/list/seg1.mp3\n#EXT-X-ENDLIST\n") }
                    }),
                )
                .route("/cf/list/seg0.mp3", get(|| async { SEG0 }))
                .route("/cf/list/seg1.mp3", get(|| async { SEG1 }))
        })
        .await;
        let http = Arc::new(HttpClient::plain(NAME));
        (
            f,
            SoundCloud::new(
                http,
                None,
                Endpoints {
                    api: base.clone(),
                    web: base,
                },
            ),
        )
    }

    #[tokio::test]
    async fn search_scrapes_client_id_and_drops_previews() {
        let (_, sc) = fake().await;
        let got = sc.search("song").await.unwrap();
        let ids: Vec<&str> = got.iter().map(|t| t.identifier.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
        assert_eq!(got[0].author, "Real Artist");
        assert_eq!(got[0].isrc, "USABC1234567");
        assert!(got[0].artwork_url.contains("t500x500"));
    }

    #[tokio::test]
    async fn rotated_client_id_is_rescraped() {
        let (f, sc) = fake().await;
        sc.search("song").await.unwrap();
        f.reject_first.store(true, Ordering::SeqCst);
        sc.search("song").await.expect("search after rotation");
        assert_eq!(f.scrapes.load(Ordering::SeqCst), 2);
    }

    async fn read(sc: &SoundCloud, id: &str) -> String {
        let t = Track {
            source: NAME.into(),
            identifier: id.into(),
            ..Default::default()
        };
        let mut s = sc.open(&t, 0).await.unwrap();
        assert_eq!(s.format, Format::mp3());
        let mut out = String::new();
        s.body.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn open_prefers_progressive_and_joins_hls_segments() {
        let (_, sc) = fake().await;
        assert_eq!(read(&sc, "1").await, AUDIO);
        assert_eq!(read(&sc, "2").await, format!("{SEG0}{SEG1}"));
        let preview = Track {
            source: NAME.into(),
            identifier: "3".into(),
            ..Default::default()
        };
        assert!(sc.open(&preview, 0).await.is_err(), "a preview-only track opened");
    }

    #[tokio::test]
    async fn links_and_sets_resolve() {
        let (_, sc) = fake().await;
        assert!(sc.matches_url("https://soundcloud.com/artist/song?si=abc"));
        assert!(sc.matches_url("https://on.soundcloud.com/xyz"));
        assert!(!sc.matches_url("https://soundcloud.com/artist"));
        let res = sc.load_url("https://soundcloud.com/artist/t1?si=x").await.unwrap();
        assert_eq!(res.kind, wavyzon_plugin::LoadType::Track);
        assert_eq!(res.tracks[0].identifier, "1");
        let set = sc.load_url("https://soundcloud.com/a/sets/s").await.unwrap();
        assert_eq!(set.kind, wavyzon_plugin::LoadType::Playlist);
        assert_eq!(set.tracks.len(), 2);
        assert_eq!(set.playlist.unwrap().name, "My Set");
    }

    #[tokio::test]
    async fn failed_scrape_is_not_retried_during_the_cooldown() {
        let (f, sc) = fake().await;
        f.web_down.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            assert!(sc.search("song").await.is_err(), "search succeeded without a client id");
        }
        assert_eq!(f.scrapes.load(Ordering::SeqCst), 1);
    }
}
