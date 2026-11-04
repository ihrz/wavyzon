//! Apple Music (metadata only): search, songs, albums, playlists and artist
//! top songs from the catalog API. The developer token is either configured or
//! scraped from the web player (the page or its `/assets/*.js` bundles), cached
//! until 6 h before its JWT `exp`, and re-scraped when refused.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use regex::Regex;
use serde::Deserialize;
use wavyzon_plugin::{Failure, LoadResult, Playlist, Source, Track};

use crate::httpx::HttpClient;
use crate::util::{bad_json, esc, query};

pub const NAME: &str = "applemusic";
pub const SEARCH_PREFIX: &str = "amsearch";
const SEARCH_LIMIT: usize = 20;
const PAGE_LIMIT: usize = 100;
const PLAYLIST_LIMIT: usize = 1000;
const ARTWORK_SIZE: &str = "1000";
const EXPIRY_MARGIN: Duration = Duration::from_secs(6 * 3600);

/// Base URLs (injectable for tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Catalog API, e.g. `https://api.music.apple.com/v1`.
    pub api: String,
    /// Web player origin, e.g. `https://music.apple.com` (token scraping and
    /// the `Origin` header).
    pub web: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.music.apple.com/v1".into(),
            web: "https://music.apple.com".into(),
        }
    }
}

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^https?://(?:beta\.)?music\.apple\.com/([a-z]{2})/(album|playlist|song|artist)/(?:[^/?#]+/)?([^/?#]+)(?:\?i=(\d+))?").unwrap())
}
fn jwt_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})""#).unwrap())
}
fn script_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?:src|href)="(/assets/[^"]+\.js)""#).unwrap())
}

/// The `exp` claim of a JWT, as seconds since the epoch.
pub fn decode_expiry(token: &str) -> Option<u64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    #[derive(Deserialize)]
    struct Claims {
        #[serde(default)]
        exp: i64,
    }
    let c: Claims = serde_json::from_slice(&raw).ok()?;
    (c.exp > 0).then_some(c.exp as u64)
}

/// The first quoted JWT in `body`.
pub fn find_token(body: &str) -> Option<String> {
    jwt_re().captures(body).map(|c| c[1].to_string())
}

/// Absolute URLs of the non-legacy `/assets/*.js` scripts of `html`.
pub fn script_urls(origin: &str, html: &str) -> Vec<String> {
    script_re()
        .captures_iter(html)
        .map(|c| c[1].to_string())
        .filter(|p| !p.contains("legacy"))
        .map(|p| format!("{origin}{p}"))
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default)]
struct TokenState {
    token: String,
    expires: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Artwork {
    #[serde(default)]
    url: String,
}

fn artwork_url(a: &Artwork) -> String {
    a.url.replace("{w}", ARTWORK_SIZE).replace("{h}", ARTWORK_SIZE)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SongAttrs {
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    album_name: String,
    #[serde(default)]
    duration_in_millis: i64,
    #[serde(default)]
    isrc: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    artwork: Artwork,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Song {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    attributes: SongAttrs,
}

impl Song {
    fn to_track(&self) -> Track {
        let a = &self.attributes;
        Track {
            source: NAME.into(),
            identifier: self.id.clone(),
            title: a.name.clone(),
            author: a.artist_name.clone(),
            album: a.album_name.clone(),
            duration_ms: a.duration_in_millis,
            isrc: a.isrc.clone(),
            artwork_url: artwork_url(&a.artwork),
            uri: a.url.clone(),
            ..Default::default()
        }
    }
}

fn songs(items: &[Song]) -> Vec<Track> {
    items
        .iter()
        .filter(|s| s.kind.is_empty() || s.kind == "songs" || s.kind == "library-songs")
        .map(Song::to_track)
        .collect()
}

#[derive(Debug, Default, Deserialize)]
struct SongPage {
    #[serde(default)]
    data: Vec<Song>,
    #[serde(default)]
    next: Option<String>,
}

/// The Apple Music metadata source.
pub struct AppleMusic {
    http: Arc<HttpClient>,
    ep: Endpoints,
    fixed: Option<String>,
    storefront: String,
    token: tokio::sync::Mutex<TokenState>,
}

impl AppleMusic {
    pub fn new(http: Arc<HttpClient>, fixed_token: Option<String>, storefront: &str, ep: Endpoints) -> Self {
        let storefront = if storefront.is_empty() {
            "us".to_string()
        } else {
            storefront.to_lowercase()
        };
        Self {
            http,
            ep,
            fixed: fixed_token.filter(|t| !t.is_empty()),
            storefront,
            token: tokio::sync::Mutex::new(TokenState::default()),
        }
    }

    async fn developer_token(&self, force: bool) -> Result<String, Failure> {
        if let Some(t) = &self.fixed {
            return Ok(t.clone());
        }
        let mut st = self.token.lock().await;
        if !force && !st.token.is_empty() && now_secs() + EXPIRY_MARGIN.as_secs() < st.expires {
            return Ok(st.token.clone());
        }
        match self.scrape().await {
            Ok(tok) => {
                st.expires = decode_expiry(&tok).unwrap_or(now_secs() + 24 * 3600);
                st.token = tok.clone();
                Ok(tok)
            }
            Err(e) if !st.token.is_empty() => {
                tracing::warn!(source = NAME, error = %e, "token scrape failed, keeping the previous token");
                Ok(st.token.clone())
            }
            Err(e) => Err(e),
        }
    }

    async fn scrape(&self) -> Result<String, Failure> {
        let missing = || {
            Failure::fault(
                NAME,
                "metadata unavailable",
                Some("apple music developer token not found in the web bundle".into()),
            )
        };
        let html = self
            .http
            .get(&format!("{}/browse", self.ep.web), &[])
            .await
            .map_err(|e| Failure::fault(NAME, "metadata unavailable", Some(e.to_string())))?;
        let html = String::from_utf8_lossy(&html);
        if let Some(t) = find_token(&html) {
            return Ok(t);
        }
        for u in script_urls(&self.ep.web, &html) {
            if let Some(t) = self
                .http
                .get(&u, &[])
                .await
                .ok()
                .and_then(|js| find_token(&String::from_utf8_lossy(&js)))
            {
                return Ok(t);
            }
        }
        Err(missing())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T, Failure> {
        let mut url = format!("{}{}", self.ep.api, path);
        if !params.is_empty() {
            url.push('?');
            url.push_str(&query(params));
        }
        for attempt in 0..2 {
            let token = self.developer_token(attempt > 0).await?;
            let auth = format!("Bearer {token}");
            match self
                .http
                .get(&url, &[("Authorization", &auth), ("Origin", &self.ep.web)])
                .await
            {
                Ok(body) => return serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e)),
                Err(e) if attempt == 0 && matches!(e.status(), Some(401 | 403)) => continue,
                Err(e) => return Err(e.to_failure(NAME)),
            }
        }
        Err(Failure::suspicious(NAME, "token refused", None))
    }

    fn catalog(&self, path: &str) -> String {
        format!("/catalog/{}{}", self.storefront, path)
    }

    async fn collection(&self, kind: &str, id: &str) -> Result<LoadResult, Failure> {
        #[derive(Deserialize, Default)]
        struct Attrs {
            #[serde(default)]
            name: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            artwork: Artwork,
        }
        #[derive(Deserialize, Default)]
        struct Rels {
            #[serde(default)]
            tracks: SongPage,
        }
        #[derive(Deserialize)]
        struct Item {
            #[serde(default)]
            attributes: Attrs,
            #[serde(default)]
            relationships: Rels,
        }
        #[derive(Deserialize)]
        struct Coll {
            #[serde(default)]
            data: Vec<Item>,
        }
        let c: Coll = self.get(&self.catalog(&format!("/{kind}/{}", esc(id))), &[]).await?;
        let Some(head) = c.data.into_iter().next() else {
            return Ok(LoadResult::empty());
        };
        let mut tracks = songs(&head.relationships.tracks.data);
        let mut next = head.relationships.tracks.next;
        while let Some(n) = next.take().filter(|n| !n.is_empty()) {
            if tracks.len() >= PLAYLIST_LIMIT {
                break;
            }
            let (path, mut q) = split_next(&n);
            q.retain(|(k, _)| k != "limit");
            q.push(("limit".into(), PAGE_LIMIT.to_string()));
            let params: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            match self.get::<SongPage>(&path, &params).await {
                Ok(page) => {
                    tracks.extend(songs(&page.data));
                    next = page.next;
                }
                Err(e) => {
                    tracing::warn!(source = NAME, error = %e, "stopped paging a collection");
                    break;
                }
            }
        }
        tracks.truncate(PLAYLIST_LIMIT);
        let total = tracks.len();
        Ok(LoadResult::playlist(
            Playlist {
                name: head.attributes.name,
                artwork_url: artwork_url(&head.attributes.artwork),
                uri: head.attributes.url,
                total,
            },
            tracks,
        ))
    }

    async fn artist_top(&self, id: &str) -> Result<LoadResult, Failure> {
        let res: SongPage = self
            .get(
                &self.catalog(&format!("/artists/{}/view/top-songs", esc(id))),
                &[("limit", "20")],
            )
            .await?;
        let tracks = songs(&res.data);
        let name = tracks.first().map(|t| t.author.clone()).unwrap_or_default();
        Ok(LoadResult::playlist(
            Playlist {
                name,
                total: tracks.len(),
                ..Default::default()
            },
            tracks,
        ))
    }
}

fn split_next(next: &str) -> (String, Vec<(String, String)>) {
    let trimmed = next.strip_prefix("/v1").unwrap_or(next);
    let (path, raw) = trimmed.split_once('?').unwrap_or((trimmed, ""));
    (
        path.to_string(),
        url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect(),
    )
}

#[async_trait]
impl Source for AppleMusic {
    fn name(&self) -> &str {
        NAME
    }

    fn search_prefix(&self) -> Option<&str> {
        Some(SEARCH_PREFIX)
    }

    async fn search(&self, q: &str) -> Result<Vec<Track>, Failure> {
        #[derive(Deserialize, Default)]
        struct Songs {
            #[serde(default)]
            data: Vec<Song>,
        }
        #[derive(Deserialize, Default)]
        struct Results {
            #[serde(default)]
            songs: Songs,
        }
        #[derive(Deserialize)]
        struct Res {
            #[serde(default)]
            results: Results,
        }
        let limit = SEARCH_LIMIT.to_string();
        let res: Res = self
            .get(
                &self.catalog("/search"),
                &[("term", q), ("types", "songs"), ("limit", &limit)],
            )
            .await?;
        Ok(songs(&res.results.songs.data))
    }

    fn matches_url(&self, url: &str) -> bool {
        link_re().is_match(url)
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let Some(m) = link_re().captures(url) else {
            return Ok(LoadResult::empty());
        };
        let kind = m[2].to_string();
        let id = m[3].to_string();
        let song_id = m.get(4).map(|s| s.as_str().to_string());
        if song_id.is_some() || kind == "song" {
            return Ok(LoadResult::single(self.track(&song_id.unwrap_or(id)).await?));
        }
        match kind.as_str() {
            "album" => self.collection("albums", &id).await,
            "playlist" => self.collection("playlists", &id).await,
            _ => self.artist_top(&id).await,
        }
    }

    async fn track(&self, id: &str) -> Result<Track, Failure> {
        let res: SongPage = self.get(&self.catalog(&format!("/songs/{}", esc(id))), &[]).await?;
        res.data
            .first()
            .map(Song::to_track)
            .ok_or_else(|| Failure::common(NAME, "not found", None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_with;
    use axum::Router;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wavyzon_plugin::LoadType;

    fn jwt(exp: u64) -> String {
        let e = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        format!(
            "{}.{}.{}",
            e(r#"{"alg":"ES256","kid":"x"}"#),
            e(&format!(r#"{{"iss":"team","exp":{exp}}}"#)),
            e("signature-bytes-here")
        )
    }

    fn song(id: &str, name: &str) -> serde_json::Value {
        json!({"id": id, "type": "songs", "attributes": {"name": name, "artistName": "Artist", "albumName": "Album", "durationInMillis": 1000,
               "isrc": "GBAAA0000001", "url": format!("https://music.apple.com/us/song/{id}"), "artwork": {"url": "https://img/{w}x{h}.jpg"}}})
    }

    #[test]
    fn token_helpers() {
        let t = jwt(4_000_000_000);
        assert_eq!(decode_expiry(&t), Some(4_000_000_000));
        assert_eq!(decode_expiry("nope"), None);
        assert_eq!(find_token(&format!(r#"x={{"token":"{t}"}}"#)), Some(t));
        let html = r#"<script src="/assets/index-abc.js"></script><link href="/assets/legacy-x.js"><script src="/assets/vendor.js">"#;
        assert_eq!(
            script_urls("https://music.apple.com", html),
            [
                "https://music.apple.com/assets/index-abc.js",
                "https://music.apple.com/assets/vendor.js"
            ]
        );
    }

    async fn fake(refuse_first: Arc<AtomicU32>, scrapes: Arc<AtomicU32>) -> AppleMusic {
        let token = jwt(now_secs() + 30 * 24 * 3600);
        let t1 = token.clone();
        let base = serve_with(move |_base| {
            let authed = move |h: &HeaderMap| h.get("authorization").and_then(|v| v.to_str().ok()) == Some(format!("Bearer {t1}").as_str());
            let a1 = authed.clone();
            let a2 = authed.clone();
            Router::new()
                .route(
                    "/browse",
                    get(move || {
                        scrapes.fetch_add(1, Ordering::SeqCst);
                        async { r#"<html><script type="module" src="/assets/index-1.js"></script></html>"# }
                    }),
                )
                .route("/assets/index-1.js", get(move || {
                    let t = token.clone();
                    async move { format!(r#"const cfg={{"developerToken":"{t}"}};"#) }
                }))
                .route(
                    "/v1/catalog/us/search",
                    get(move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| {
                        let ok = a1(&h) && refuse_first.fetch_add(1, Ordering::SeqCst) > 0;
                        async move {
                            if !ok {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                            axum::Json(json!({"results": {"songs": {"data": [song("1", &q["term"]), {"id": "v", "type": "music-videos"}]}}})).into_response()
                        }
                    }),
                )
                .route("/v1/catalog/us/songs/{id}", get(move |h: HeaderMap| {
                    let ok = a2(&h);
                    async move { if ok { axum::Json(json!({"data": [song("42", "Single")]})).into_response() } else { StatusCode::UNAUTHORIZED.into_response() } }
                }))
                .route(
                    "/v1/catalog/us/albums/{id}",
                    get(|| async {
                        axum::Json(json!({"data": [{"attributes": {"name": "LP", "url": "u", "artwork": {"url": "https://img/{w}.jpg"}},
                            "relationships": {"tracks": {"data": [song("a1", "One")], "next": "/v1/catalog/us/albums/9/tracks?offset=1"}}}]}))
                    }),
                )
                .route(
                    "/v1/catalog/us/albums/{id}/tracks",
                    get(|Query(q): Query<HashMap<String, String>>| async move {
                        assert_eq!(q.get("limit").map(String::as_str), Some("100"));
                        axum::Json(json!({"data": [song("a2", "Two")]}))
                    }),
                )
        })
        .await;
        AppleMusic::new(
            Arc::new(HttpClient::plain(NAME)),
            None,
            "us",
            Endpoints {
                api: format!("{base}/v1"),
                web: base,
            },
        )
    }

    #[tokio::test]
    async fn scrapes_token_from_bundle_and_retries_once_when_refused() {
        let scrapes = Arc::new(AtomicU32::new(0));
        let am = fake(Arc::new(AtomicU32::new(0)), scrapes.clone()).await;
        let got = am.search("hello").await.unwrap();
        assert_eq!(got.len(), 1, "non-songs are dropped");
        assert_eq!(got[0].title, "hello");
        assert_eq!(got[0].artwork_url, "https://img/1000x1000.jpg");
        assert_eq!(scrapes.load(Ordering::SeqCst), 2, "refused token forces one re-scrape");
        am.track("42").await.unwrap();
        assert_eq!(scrapes.load(Ordering::SeqCst), 2, "valid token is reused");
    }

    #[tokio::test]
    async fn links_resolve_with_paging() {
        let am = fake(Arc::new(AtomicU32::new(1)), Arc::new(AtomicU32::new(0))).await;
        assert!(am.matches_url("https://music.apple.com/us/album/lp/9?i=42"));
        let single = am.load_url("https://music.apple.com/us/album/lp/9?i=42").await.unwrap();
        assert_eq!(single.kind, LoadType::Track);
        assert_eq!(single.tracks[0].identifier, "42");
        let album = am.load_url("https://music.apple.com/us/album/lp/9").await.unwrap();
        assert_eq!(album.kind, LoadType::Playlist);
        let ids: Vec<&str> = album.tracks.iter().map(|t| t.identifier.as_str()).collect();
        assert_eq!(ids, ["a1", "a2"]);
        assert_eq!(album.playlist.unwrap().name, "LP");
    }
}
