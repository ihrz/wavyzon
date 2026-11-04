//! Spotify (metadata only): search, tracks, albums, playlists (up to 1 000
//! tracks), `spotify:` URIs and `spotify.link` short links, through the Web
//! API with client-credentials auth. Tracks are mirrored to a playback source
//! by the resolver.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use regex::Regex;
use reqwest::Method;
use serde::Deserialize;
use wavyzon_plugin::{Failure, IsrcLookup, LoadResult, Playlist, Source, Track};

use crate::httpx::{HttpClient, Req};
use crate::util::{bad_json, esc, query};

pub const NAME: &str = "spotify";
pub const SEARCH_PREFIX: &str = "spsearch";
const SEARCH_LIMIT: usize = 10;
const PAGE_LIMIT: usize = 100;
const PLAYLIST_LIMIT: usize = 1000;
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// API base URLs (injectable for tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub api: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.spotify.com/v1".into(),
            token: "https://accounts.spotify.com/api/token".into(),
        }
    }
}

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^https?://open\.spotify\.com/(?:intl-[a-z]{2}(?:-[a-z]{2})?/)?(track|album|playlist)/([A-Za-z0-9]{22})",
        )
        .unwrap()
    })
}
fn uri_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^spotify:(track|album|playlist):([A-Za-z0-9]{22})$").unwrap())
}
fn short_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^https?://spotify\.link/").unwrap())
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Image {
    #[serde(default)]
    url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Artist {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Album {
    #[serde(default)]
    name: String,
    #[serde(default)]
    images: Vec<Image>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ExternalIds {
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ExternalUrls {
    #[serde(default)]
    spotify: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiTrack {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    duration_ms: i64,
    #[serde(default)]
    is_local: bool,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    album: Option<Album>,
    #[serde(default)]
    external_ids: ExternalIds,
    #[serde(default)]
    external_urls: ExternalUrls,
}

impl ApiTrack {
    fn to_track(&self, fallback: Option<&Album>) -> Track {
        let id = self.id.clone().unwrap_or_default();
        let album = self.album.as_ref().or(fallback);
        Track {
            source: NAME.into(),
            uri: self
                .external_urls
                .spotify
                .clone()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| format!("https://open.spotify.com/track/{id}")),
            identifier: id,
            title: self.name.clone(),
            author: self
                .artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            duration_ms: self.duration_ms,
            isrc: self.external_ids.isrc.clone().unwrap_or_default(),
            album: album.map(|a| a.name.clone()).unwrap_or_default(),
            artwork_url: album
                .and_then(|a| a.images.first())
                .map(|i| i.url.clone())
                .unwrap_or_default(),
            ..Default::default()
        }
    }
}

fn convert(items: &[ApiTrack], fallback: Option<&Album>) -> Vec<Track> {
    items
        .iter()
        .filter(|t| t.id.as_deref().is_some_and(|i| !i.is_empty()) && !t.is_local)
        .map(|t| t.to_track(fallback))
        .collect()
}

#[derive(Default)]
struct Token {
    value: String,
    expires: Option<Instant>,
}

/// The Spotify metadata source.
pub struct Spotify {
    http: Arc<HttpClient>,
    ep: Endpoints,
    client_id: String,
    client_secret: String,
    token: tokio::sync::Mutex<Token>,
}

impl Spotify {
    pub fn new(http: Arc<HttpClient>, client_id: &str, client_secret: &str, ep: Endpoints) -> Self {
        Self {
            http,
            ep,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token: tokio::sync::Mutex::new(Token::default()),
        }
    }

    async fn access_token(&self) -> Result<String, Failure> {
        if self.client_id.is_empty() || self.client_secret.is_empty() {
            return Err(Failure::fault(
                NAME,
                "metadata unavailable",
                Some("SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET are not set".into()),
            ));
        }
        let mut tok = self.token.lock().await;
        if !tok.value.is_empty() && tok.expires.is_some_and(|e| Instant::now() < e) {
            return Ok(tok.value.clone());
        }
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.client_id, self.client_secret));
        let req = Req::new(Method::POST, &self.ep.token)
            .map_err(|e| e.to_failure(NAME))?
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Authorization", &format!("Basic {basic}"))
            .body("grant_type=client_credentials");
        let body = self.http.fetch(&req).await.map_err(|e| e.to_failure(NAME))?;
        #[derive(Deserialize)]
        struct Tok {
            access_token: String,
            #[serde(default)]
            expires_in: u64,
        }
        let t: Tok = serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e))?;
        tok.value = t.access_token;
        tok.expires = Some(Instant::now() + Duration::from_secs(t.expires_in).saturating_sub(TOKEN_MARGIN));
        Ok(tok.value.clone())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T, Failure> {
        let token = self.access_token().await?;
        let mut url = format!("{}{}", self.ep.api, path);
        if !params.is_empty() {
            url.push('?');
            url.push_str(&query(params));
        }
        match self
            .http
            .get(&url, &[("Authorization", &format!("Bearer {token}"))])
            .await
        {
            Ok(body) => serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e)),
            Err(e) => {
                if e.status() == Some(401) {
                    self.token.lock().await.value.clear();
                }
                Err(e.to_failure(NAME))
            }
        }
    }

    async fn album(&self, id: &str) -> Result<LoadResult, Failure> {
        #[derive(Deserialize)]
        struct Tracks {
            #[serde(default)]
            items: Vec<ApiTrack>,
            #[serde(default)]
            total: usize,
        }
        #[derive(Deserialize)]
        struct Al {
            #[serde(flatten)]
            album: Album,
            #[serde(default)]
            external_urls: ExternalUrls,
            tracks: Tracks,
        }
        let al: Al = self.get(&format!("/albums/{}", esc(id)), &[]).await?;
        let tracks = convert(&al.tracks.items, Some(&al.album));
        let playlist = Playlist {
            name: al.album.name.clone(),
            artwork_url: al.album.images.first().map(|i| i.url.clone()).unwrap_or_default(),
            uri: al.external_urls.spotify.unwrap_or_default(),
            total: al.tracks.total,
        };
        Ok(LoadResult::playlist(playlist, tracks))
    }

    async fn playlist(&self, id: &str) -> Result<LoadResult, Failure> {
        #[derive(Deserialize)]
        struct Meta {
            #[serde(default)]
            name: String,
            #[serde(default)]
            images: Option<Vec<Image>>,
            #[serde(default)]
            external_urls: ExternalUrls,
        }
        #[derive(Deserialize)]
        struct Item {
            #[serde(default)]
            track: Option<ApiTrack>,
        }
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            items: Vec<Item>,
            #[serde(default)]
            total: usize,
            #[serde(default)]
            next: Option<String>,
        }
        let path = format!("/playlists/{}", esc(id));
        let meta: Meta = self.get(&path, &[("fields", "name,images,external_urls")]).await?;
        let mut all = Vec::new();
        let mut total = 0;
        let mut offset = 0;
        while offset < PLAYLIST_LIMIT {
            let (o, l) = (offset.to_string(), PAGE_LIMIT.to_string());
            let page: Page = self
                .get(&format!("{path}/tracks"), &[("offset", &o), ("limit", &l)])
                .await?;
            total = page.total;
            let items: Vec<ApiTrack> = page.items.into_iter().filter_map(|i| i.track).collect();
            all.extend(convert(&items, None));
            if page.next.as_deref().is_none_or(str::is_empty) {
                break;
            }
            offset += PAGE_LIMIT;
        }
        let playlist = Playlist {
            name: meta.name,
            artwork_url: meta
                .images
                .unwrap_or_default()
                .first()
                .map(|i| i.url.clone())
                .unwrap_or_default(),
            uri: meta.external_urls.spotify.unwrap_or_default(),
            total,
        };
        Ok(LoadResult::playlist(playlist, all))
    }

    /// Follow a `spotify.link` short link to its `open.spotify.com` target.
    pub async fn expand(&self, short: &str) -> Result<String, Failure> {
        match self.http.location(short).await.map_err(|e| e.to_failure(NAME))? {
            Some(loc) if !loc.is_empty() => Ok(loc),
            _ => Err(Failure::common(NAME, "short link did not redirect", None)),
        }
    }
}

#[async_trait]
impl Source for Spotify {
    fn name(&self) -> &str {
        NAME
    }

    fn search_prefix(&self) -> Option<&str> {
        Some(SEARCH_PREFIX)
    }

    async fn search(&self, q: &str) -> Result<Vec<Track>, Failure> {
        #[derive(Deserialize)]
        struct Items {
            #[serde(default)]
            items: Vec<ApiTrack>,
        }
        #[derive(Deserialize)]
        struct Res {
            tracks: Items,
        }
        let limit = SEARCH_LIMIT.to_string();
        let res: Res = self
            .get("/search", &[("q", q), ("type", "track"), ("limit", &limit)])
            .await?;
        Ok(convert(&res.tracks.items, None))
    }

    fn matches_url(&self, url: &str) -> bool {
        link_re().is_match(url) || uri_re().is_match(url) || short_re().is_match(url)
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let mut url = url.to_string();
        if short_re().is_match(&url) {
            url = self.expand(&url).await?;
        }
        let Some(m) = link_re().captures(&url).or_else(|| uri_re().captures(&url)) else {
            return Ok(LoadResult::empty());
        };
        let (kind, id) = (m[1].to_string(), m[2].to_string());
        match kind.as_str() {
            "track" => Ok(LoadResult::single(self.track(&id).await?)),
            "album" => self.album(&id).await,
            _ => self.playlist(&id).await,
        }
    }

    async fn track(&self, id: &str) -> Result<Track, Failure> {
        let t: ApiTrack = self.get(&format!("/tracks/{}", esc(id)), &[]).await?;
        Ok(t.to_track(None))
    }

    fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;
    use axum::Router;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wavyzon_plugin::LoadType;

    const ID: &str = "4uLU6hMCjMI75M1A2tKUQC";

    fn track_json(id: &str, name: &str) -> serde_json::Value {
        json!({"id": id, "name": name, "duration_ms": 213000, "artists": [{"name": "A"}, {"name": "B"}],
               "album": {"name": "Alb", "images": [{"url": "https://img/1.jpg"}]},
               "external_ids": {"isrc": "USRC17607839"}, "external_urls": {"spotify": format!("https://open.spotify.com/track/{id}")}})
    }

    async fn fake(tokens: Arc<AtomicU32>) -> Spotify {
        let authed = |h: &HeaderMap| h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer tok");
        let app = Router::new()
            .route(
                "/token",
                post(move |h: HeaderMap, body: String| {
                    let tokens = tokens.clone();
                    async move {
                        tokens.fetch_add(1, Ordering::SeqCst);
                        let ok = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Basic aWQ6c2VjcmV0") && body == "grant_type=client_credentials";
                        if !ok {
                            return StatusCode::BAD_REQUEST.into_response();
                        }
                        axum::Json(json!({"access_token": "tok", "expires_in": 3600})).into_response()
                    }
                }),
            )
            .route(
                "/v1/search",
                get(move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if !authed(&h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    assert_eq!(q.get("type").map(String::as_str), Some("track"));
                    axum::Json(json!({"tracks": {"items": [track_json(ID, &q["q"]), {"id": null, "name": "local", "is_local": true}]}})).into_response()
                }),
            )
            .route(&format!("/v1/tracks/{ID}"), get(|| async { axum::Json(track_json(ID, "One")) }))
            .route(
                "/v1/albums/{id}",
                get(|| async {
                    axum::Json(json!({"name": "Album", "images": [{"url": "https://img/a.jpg"}], "external_urls": {"spotify": "https://open.spotify.com/album/x"},
                        "tracks": {"total": 2, "items": [{"id": "t1", "name": "First", "artists": [{"name": "A"}]}, {"id": "t2", "name": "Second", "artists": [{"name": "A"}]}]}}))
                }),
            )
            .route("/v1/playlists/{id}", get(|| async { axum::Json(json!({"name": "Mix", "images": null, "external_urls": {"spotify": "u"}})) }))
            .route(
                "/v1/playlists/{id}/tracks",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let offset: usize = q["offset"].parse().unwrap();
                    if offset == 0 {
                        axum::Json(json!({"total": 101, "next": "more", "items": [{"track": track_json("p1", "P1")}, {"track": null}]}))
                    } else {
                        axum::Json(json!({"total": 101, "next": null, "items": [{"track": track_json("p2", "P2")}]}))
                    }
                }),
            )
            .route("/short", get(|| async { axum::response::Redirect::temporary(&format!("https://open.spotify.com/track/{ID}?si=1")) }));
        let base = serve(app).await;
        Spotify::new(
            Arc::new(HttpClient::plain(NAME)),
            "id",
            "secret",
            Endpoints {
                api: format!("{base}/v1"),
                token: format!("{base}/token"),
            },
        )
    }

    #[tokio::test]
    async fn search_maps_tracks_and_reuses_the_token() {
        let tokens = Arc::new(AtomicU32::new(0));
        let sp = fake(tokens.clone()).await;
        let got = sp.search("hello").await.unwrap();
        assert_eq!(got.len(), 1, "local tracks are dropped");
        assert_eq!(got[0].title, "hello");
        assert_eq!(got[0].author, "A, B");
        assert_eq!(got[0].isrc, "USRC17607839");
        assert_eq!(got[0].album, "Alb");
        sp.search("again").await.unwrap();
        assert_eq!(tokens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn links_uris_albums_and_paged_playlists() {
        let sp = fake(Arc::new(AtomicU32::new(0))).await;
        assert!(sp.matches_url(&format!("https://open.spotify.com/intl-fr/track/{ID}")));
        assert!(sp.matches_url(&format!("spotify:track:{ID}")));
        assert!(sp.matches_url("https://spotify.link/abc"));
        assert!(!sp.matches_url("https://open.spotify.com/artist/x"));
        let res = sp.load_url(&format!("spotify:track:{ID}")).await.unwrap();
        assert_eq!(res.tracks[0].identifier, ID);
        let al = sp
            .load_url(&format!("https://open.spotify.com/album/{ID}"))
            .await
            .unwrap();
        assert_eq!(al.kind, LoadType::Playlist);
        assert_eq!(al.tracks[1].album, "Album", "album tracks inherit the album");
        assert_eq!(al.tracks[1].artwork_url, "https://img/a.jpg");
        let pl = sp
            .load_url(&format!("https://open.spotify.com/playlist/{ID}"))
            .await
            .unwrap();
        assert_eq!(pl.tracks.len(), 2);
        assert_eq!(pl.playlist.unwrap().total, 101);
    }

    #[tokio::test]
    async fn missing_credentials_is_a_fault_and_short_links_expand() {
        let sp = fake(Arc::new(AtomicU32::new(0))).await;
        let base = sp.ep.token.trim_end_matches("/token").to_string();
        assert_eq!(
            sp.expand(&format!("{base}/short")).await.unwrap(),
            format!("https://open.spotify.com/track/{ID}?si=1")
        );
        let none = Spotify::new(Arc::new(HttpClient::plain(NAME)), "", "", Endpoints::default());
        assert_eq!(
            none.search("x").await.unwrap_err().severity,
            wavyzon_plugin::Severity::Fault
        );
    }
}
