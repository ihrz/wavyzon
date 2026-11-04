//! Deezer **public catalog** (metadata only): search, tracks, albums,
//! playlists (up to 1 000 tracks), artist top tracks, short links, ISRC
//! lookups (`dzisrc:`) and artist-radio recommendations, all through the
//! unauthenticated `api.deezer.com` API.
//!
//! Unlike Cadence, Wavyzon does **not** stream from Deezer: there is no
//! account login, media URL fetching or stream decryption. Deezer tracks are
//! mirrored to a playback source by the resolver, like Spotify and Apple
//! Music. (Cadence's logged-in "track mix" recommendations are therefore
//! replaced by the public artist radio.)

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use wavyzon_plugin::{Failure, IsrcLookup, LoadResult, Playlist, Recommender, Source, Track};

use crate::cache::Lru;
use crate::httpx::HttpClient;
use crate::matching::normalize_isrc;
use crate::util::{bad_json, esc, query};

pub const NAME: &str = "deezer";
pub const SEARCH_PREFIX: &str = "dzsearch";
/// ISRC lookup prefix (handled by the resolver for any `<x>isrc:` prefix).
pub const ISRC_PREFIX: &str = "dzisrc:";
const SEARCH_LIMIT: usize = 20;
const PLAYLIST_PAGE: usize = 100;
const PLAYLIST_LIMIT: usize = 1000;
const ERR_NO_DATA: i64 = 800;
const ISRC_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const RADIO_ENTRIES: usize = 20_000;
const RADIO_TTL: Duration = Duration::from_secs(6 * 3600);
const RADIO_MAX_LIMIT: usize = 50;
const MAX_SHORT_HOPS: usize = 5;
const LANG: (&str, &str) = ("Accept-Language", "en-US,en;q=0.9");

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^https?://(?:www\.)?deezer\.com/(?:[a-z]{2}(?:-[a-z]{2})?/)?(track|album|playlist|artist)/(\d+)")
            .unwrap()
    })
}
fn short_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^https?://(?:deezer\.page\.link|link\.deezer\.com)/").unwrap())
}

#[derive(Debug, Clone, Deserialize)]
struct ApiError {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiArtist {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiAlbum {
    #[serde(default)]
    title: String,
    #[serde(default)]
    cover_xl: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiTrack {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    isrc: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    readable: Option<bool>,
    #[serde(default)]
    artist: ApiArtist,
    #[serde(default)]
    album: ApiAlbum,
}

impl ApiTrack {
    fn to_track(&self, fallback_art: &str) -> Track {
        let art = self
            .album
            .cover_xl
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| fallback_art.to_string());
        Track {
            source: NAME.into(),
            identifier: self.id.to_string(),
            title: self.title.clone(),
            author: self.artist.name.clone(),
            album: self.album.title.clone(),
            duration_ms: self.duration * 1000,
            isrc: self.isrc.clone(),
            artwork_url: art,
            uri: if self.link.is_empty() {
                format!("https://www.deezer.com/track/{}", self.id)
            } else {
                self.link.clone()
            },
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct TrackPage {
    #[serde(default)]
    data: Vec<ApiTrack>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Collection {
    #[serde(default)]
    title: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    cover_xl: Option<String>,
    #[serde(default)]
    picture_xl: Option<String>,
    #[serde(default)]
    nb_tracks: usize,
    #[serde(default)]
    tracks: TrackPage,
}

fn readable_only(tracks: &[ApiTrack], art: &str) -> Vec<Track> {
    tracks
        .iter()
        .filter(|t| t.readable != Some(false))
        .map(|t| t.to_track(art))
        .collect()
}

/// Drop `seed` and duplicates.
fn without_seed(tracks: Vec<Track>, seed: &str) -> Vec<Track> {
    let mut seen = std::collections::HashSet::from([seed.to_string()]);
    tracks
        .into_iter()
        .filter(|t| seen.insert(t.identifier.clone()))
        .collect()
}

fn cap_tracks(mut tracks: Vec<Track>, limit: usize) -> Vec<Track> {
    tracks.truncate(limit);
    tracks
}

/// The Deezer catalog source.
pub struct Deezer {
    http: Arc<HttpClient>,
    api_base: String,
    isrc: Lru<String, Track>,
    radio: Lru<String, Vec<Track>>,
}

impl Deezer {
    /// `api_base` defaults to `https://api.deezer.com/2.0` when empty.
    pub fn new(http: Arc<HttpClient>, api_base: &str) -> Self {
        let api_base = if api_base.is_empty() {
            "https://api.deezer.com/2.0".to_string()
        } else {
            api_base.trim_end_matches('/').to_string()
        };
        Self {
            http,
            api_base,
            isrc: Lru::new(200_000, ISRC_CACHE_TTL),
            radio: Lru::new(RADIO_ENTRIES, RADIO_TTL),
        }
    }

    async fn api<T: serde::de::DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T, Failure> {
        let mut url = format!("{}{}", self.api_base, path);
        if !params.is_empty() {
            url.push('?');
            url.push_str(&query(params));
        }
        let body = self.http.get(&url, &[LANG]).await.map_err(|e| e.to_failure(NAME))?;
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default)]
            error: Option<ApiError>,
        }
        if let Ok(Probe { error: Some(e) }) = serde_json::from_slice::<Probe>(&body) {
            if e.code == ERR_NO_DATA {
                return Err(Failure::common(NAME, "not found", None));
            }
            return Err(Failure::suspicious(
                NAME,
                "api error",
                Some(format!("{} ({}): {}", e.kind, e.code, e.message)),
            ));
        }
        serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e))
    }

    async fn album(&self, id: &str) -> Result<LoadResult, Failure> {
        let c: Collection = self.api(&format!("/album/{}", esc(id)), &[]).await?;
        let cover = c.cover_xl.clone().unwrap_or_default();
        let mut tracks = readable_only(&c.tracks.data, &cover);
        for t in &mut tracks {
            if t.album.is_empty() {
                t.album = c.title.clone();
            }
        }
        let total = tracks.len();
        Ok(LoadResult::playlist(
            Playlist {
                name: c.title,
                artwork_url: cover,
                uri: c.link,
                total,
            },
            tracks,
        ))
    }

    async fn playlist(&self, id: &str) -> Result<LoadResult, Failure> {
        let path = format!("/playlist/{}", esc(id));
        let c: Collection = self.api(&path, &[("limit", "0")]).await?;
        let mut all = Vec::new();
        let mut index = 0;
        while index < PLAYLIST_LIMIT {
            let (i, l) = (index.to_string(), PLAYLIST_PAGE.to_string());
            let page: TrackPage = self
                .api(&format!("{path}/tracks"), &[("index", &i), ("limit", &l)])
                .await?;
            all.extend(readable_only(&page.data, ""));
            if page.next.as_deref().is_none_or(str::is_empty) || page.data.is_empty() {
                break;
            }
            index += PLAYLIST_PAGE;
        }
        Ok(LoadResult::playlist(
            Playlist {
                name: c.title,
                artwork_url: c.picture_xl.unwrap_or_default(),
                uri: c.link,
                total: c.nb_tracks,
            },
            all,
        ))
    }

    async fn artist_top(&self, id: &str) -> Result<LoadResult, Failure> {
        let c: Collection = self.api(&format!("/artist/{}", esc(id)), &[]).await?;
        let page: TrackPage = self
            .api(&format!("/artist/{}/top", esc(id)), &[("limit", "50")])
            .await?;
        let total = page.data.len();
        Ok(LoadResult::playlist(
            Playlist {
                name: c.name,
                artwork_url: c.picture_xl.unwrap_or_default(),
                uri: c.link,
                total,
            },
            readable_only(&page.data, ""),
        ))
    }

    /// Follow Deezer short links (several hops at most) to a
    /// `deezer.com/<kind>/<id>` URL without query.
    pub async fn expand(&self, short: &str) -> Result<String, Failure> {
        let mut cur = short.to_string();
        for _ in 0..MAX_SHORT_HOPS {
            let loc = self.http.location(&cur).await.map_err(|e| e.to_failure(NAME))?;
            let Some(loc) = loc.and_then(|l| url::Url::parse(&l).ok()) else {
                return Err(Failure::common(NAME, "short link did not redirect", None));
            };
            let host = loc.host_str().unwrap_or("");
            if host.contains("deezer.page.link") || host.contains("link.deezer.com") {
                cur = loc.to_string();
                continue;
            }
            return Ok(format!("{}://{}{}", loc.scheme(), host, loc.path()));
        }
        Err(Failure::common(NAME, "short link redirects too many times", None))
    }

    async fn seed_id(&self, identifier: &str) -> Result<String, Failure> {
        if let Some(isrc) = identifier.strip_prefix("isrc:") {
            return Ok(self.by_isrc(&normalize_isrc(isrc)).await?.identifier);
        }
        if identifier.parse::<i64>().is_err() {
            return Err(Failure::common(NAME, "not a deezer track id", None));
        }
        Ok(identifier.to_string())
    }

    async fn artist_radio(&self, seed: &str) -> Result<Vec<Track>, Failure> {
        let t: ApiTrack = self.api(&format!("/track/{}", esc(seed)), &[]).await?;
        if t.artist.id == 0 {
            return Err(Failure::common(NAME, "track has no artist", None));
        }
        let limit = RADIO_MAX_LIMIT.to_string();
        let page: TrackPage = self
            .api(&format!("/artist/{}/radio", t.artist.id), &[("limit", &limit)])
            .await?;
        Ok(readable_only(&page.data, ""))
    }
}

#[async_trait]
impl Source for Deezer {
    fn name(&self) -> &str {
        NAME
    }

    fn search_prefix(&self) -> Option<&str> {
        Some(SEARCH_PREFIX)
    }

    async fn search(&self, q: &str) -> Result<Vec<Track>, Failure> {
        if let Some(isrc) = q.strip_prefix(ISRC_PREFIX) {
            return Ok(vec![self.by_isrc(isrc).await?]);
        }
        let limit = SEARCH_LIMIT.to_string();
        let page: TrackPage = self.api("/search", &[("q", q), ("limit", &limit)]).await?;
        Ok(readable_only(&page.data, ""))
    }

    fn matches_url(&self, url: &str) -> bool {
        link_re().is_match(url) || short_re().is_match(url)
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let url = if short_re().is_match(url) {
            self.expand(url).await?
        } else {
            url.to_string()
        };
        let Some(m) = link_re().captures(&url) else {
            return Ok(LoadResult::empty());
        };
        let id = m[2].to_string();
        match &m[1] {
            "track" => Ok(LoadResult::single(self.track(&id).await?)),
            "album" => self.album(&id).await,
            "playlist" => self.playlist(&id).await,
            _ => self.artist_top(&id).await,
        }
    }

    async fn track(&self, id: &str) -> Result<Track, Failure> {
        let t: ApiTrack = self.api(&format!("/track/{}", esc(id)), &[]).await?;
        if t.id == 0 {
            return Err(Failure::common(NAME, "not found", None));
        }
        Ok(t.to_track(""))
    }

    fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
        Some(self)
    }

    fn recommender(&self) -> Option<&dyn Recommender> {
        Some(self)
    }
}

#[async_trait]
impl IsrcLookup for Deezer {
    async fn by_isrc(&self, isrc: &str) -> Result<Track, Failure> {
        let isrc = normalize_isrc(isrc);
        if let Some(hit) = self.isrc.get(&isrc) {
            return Ok(hit);
        }
        let t = self.track(&format!("isrc:{isrc}")).await?;
        self.isrc.set(isrc, t.clone());
        Ok(t)
    }
}

#[async_trait]
impl Recommender for Deezer {
    /// Artist radio of the seed track. `identifier` is a Deezer track id or
    /// `isrc:<ISRC>`.
    async fn recommend(&self, identifier: &str, limit: usize) -> Result<Vec<Track>, Failure> {
        let limit = limit.clamp(1, RADIO_MAX_LIMIT);
        let seed = self.seed_id(identifier.trim()).await?;
        if let Some(hit) = self.radio.get(&seed) {
            return Ok(cap_tracks(hit, limit));
        }
        let tracks = without_seed(self.artist_radio(&seed).await?, &seed);
        if tracks.is_empty() {
            return Ok(vec![]);
        }
        self.radio.set(seed, tracks.clone());
        Ok(cap_tracks(tracks, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;
    use axum::Router;
    use axum::extract::{Path, Query};
    use axum::routing::get;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wavyzon_plugin::LoadType;

    fn t(id: i64, title: &str) -> serde_json::Value {
        json!({"id": id, "title": title, "duration": 200, "isrc": "GBAAA0000001", "link": format!("https://www.deezer.com/track/{id}"),
               "artist": {"id": 27, "name": "Daft Punk"}, "album": {"title": "Discovery", "cover_xl": "https://cover/xl.jpg"}})
    }

    async fn fake(calls: Arc<AtomicU32>) -> (Deezer, String) {
        let app = Router::new()
            .route(
                "/search",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    axum::Json(json!({"data": [t(1, &q["q"]), {"id": 2, "title": "gone", "readable": false}]}))
                }),
            )
            .route(
                "/track/{id}",
                get(move |Path(id): Path<String>| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        match id.as_str() {
                            "isrc:GBAAA0000001" | "3135553" => axum::Json(t(3135553, "One More Time")),
                            _ => axum::Json(json!({"error": {"type": "DataException", "message": "no data", "code": 800}})),
                        }
                    }
                }),
            )
            .route("/album/{id}", get(|| async { axum::Json(json!({"title": "Discovery", "link": "l", "cover_xl": "c", "tracks": {"data": [{"id": 5, "title": "A", "artist": {"name": "x"}}]}})) }))
            .route("/playlist/{id}", get(|| async { axum::Json(json!({"title": "PL", "nb_tracks": 3, "picture_xl": "p", "link": "l"})) }))
            .route(
                "/playlist/{id}/tracks",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q["index"] == "0" { axum::Json(json!({"data": [t(10, "a"), t(11, "b")], "next": "x"})) } else { axum::Json(json!({"data": [t(12, "c")]})) }
                }),
            )
            .route("/artist/{id}", get(|| async { axum::Json(json!({"name": "Daft Punk", "picture_xl": "p", "link": "l"})) }))
            .route("/artist/{id}/top", get(|| async { axum::Json(json!({"data": [t(20, "top")]})) }))
            .route("/artist/27/radio", get(|| async { axum::Json(json!({"data": [t(3135553, "seed"), t(7, "x"), t(7, "x"), t(8, "y"), t(9, "z")]})) }))
            .route("/short", get(|| async { axum::response::Redirect::temporary("https://www.deezer.com/fr/track/3135553?utm=1") }));
        let base = serve(app).await;
        (Deezer::new(Arc::new(HttpClient::plain(NAME)), &base), base)
    }

    #[tokio::test]
    async fn search_tracks_and_isrc() {
        let calls = Arc::new(AtomicU32::new(0));
        let (dz, _) = fake(calls.clone()).await;
        let got = dz.search("daft").await.unwrap();
        assert_eq!(got.len(), 1, "unreadable tracks are dropped");
        assert_eq!(got[0].duration_ms, 200_000);
        assert_eq!(got[0].artwork_url, "https://cover/xl.jpg");
        assert_eq!(dz.by_isrc("gb-aaa-00-00001").await.unwrap().identifier, "3135553");
        dz.by_isrc("GBAAA0000001").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "isrc hits are cached");
        assert_eq!(dz.search("dzisrc:GBAAA0000001").await.unwrap()[0].identifier, "3135553");
        let err = dz.track("404").await.unwrap_err();
        assert_eq!(err.severity, wavyzon_plugin::Severity::Common);
    }

    #[tokio::test]
    async fn links_resolve() {
        let (dz, base) = fake(Arc::new(AtomicU32::new(0))).await;
        assert!(dz.matches_url("https://www.deezer.com/en/album/302127"));
        assert!(dz.matches_url("https://link.deezer.com/s/abc"));
        let al = dz.load_url("https://www.deezer.com/album/1").await.unwrap();
        assert_eq!(al.tracks[0].album, "Discovery");
        assert_eq!(al.tracks[0].artwork_url, "c");
        let pl = dz.load_url("https://www.deezer.com/playlist/1").await.unwrap();
        assert_eq!(pl.kind, LoadType::Playlist);
        assert_eq!(pl.tracks.len(), 3);
        assert_eq!(pl.playlist.unwrap().total, 3);
        let ar = dz.load_url("https://deezer.com/artist/27").await.unwrap();
        assert_eq!(ar.playlist.unwrap().name, "Daft Punk");
        assert_eq!(
            dz.expand(&format!("{base}/short")).await.unwrap(),
            "https://www.deezer.com/fr/track/3135553"
        );
    }

    #[tokio::test]
    async fn recommend_uses_artist_radio_without_seed_and_caches() {
        let calls = Arc::new(AtomicU32::new(0));
        let (dz, _) = fake(calls.clone()).await;
        let got = dz.recommend("3135553", 2).await.unwrap();
        let ids: Vec<&str> = got.iter().map(|t| t.identifier.as_str()).collect();
        assert_eq!(ids, ["7", "8"]);
        let again = dz.recommend("isrc:GBAAA0000001", 10).await.unwrap();
        assert_eq!(again.len(), 3);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "second call served from the radio cache (one isrc lookup only)"
        );
    }

    #[tokio::test]
    async fn recommend_rejects_a_seed_that_is_not_a_deezer_id() {
        let dz = Deezer::new(Arc::new(HttpClient::plain(NAME)), "http://127.0.0.1:9");
        assert!(dz.recommend("soundcloud-track", 10).await.is_err());
        dz.radio.set(
            "42".into(),
            vec![
                Track {
                    identifier: "7".into(),
                    ..Default::default()
                },
                Track {
                    identifier: "8".into(),
                    ..Default::default()
                },
            ],
        );
        let got = dz.recommend("42", 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].identifier, "7");
    }

    #[test]
    fn without_seed_and_cap() {
        let mk = |i: &str| Track {
            identifier: i.into(),
            ..Default::default()
        };
        let got = without_seed(vec![mk("1"), mk("2"), mk("2"), mk("3")], "1");
        assert_eq!(
            got.iter().map(|t| t.identifier.as_str()).collect::<Vec<_>>(),
            ["2", "3"]
        );
        assert_eq!(cap_tracks(vec![mk("1"), mk("2"), mk("3")], 2).len(), 2);
        assert_eq!(cap_tracks(vec![mk("1")], 10).len(), 1);
    }
}
