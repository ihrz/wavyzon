//! Deezer: the public catalog **and** real streaming, ported from Cadence's
//! `internal/source/deezer`.
//!
//! Metadata (search `dzsearch:`, tracks, albums, playlists up to 1 000
//! tracks, artist top tracks, short links, ISRC lookups `dzisrc:`,
//! recommendations `dzrec:`) goes through the unauthenticated
//! `api.deezer.com`. Streaming needs an account: [`session`] exchanges the
//! `DEEZER_ARL` cookie for stream tokens and a CDN URL, [`stripe`] decrypts
//! the `BF_CBC_STRIPE` chunks, and [`seek`] opens byte ranges mid-track.
//! Recommendations use the logged-in "track mix", falling back to the public
//! artist radio.
//!
//! Without `DEEZER_ARL` or `DEEZER_MASTER_KEY` the source stays
//! metadata-only: [`Deezer::playable`] returns `None` and the resolver
//! mirrors its tracks to another playback source, exactly like Spotify.

pub mod accounts;
pub mod seek;
pub mod session;
pub mod stripe;

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use wavyzon_plugin::{Failure, Format, IsrcLookup, LoadResult, Playable, Playlist, Recommender, Source, Stream, Track};

use crate::cache::Lru;
use crate::httpx::{HttpClient, Req};
use crate::matching::normalize_isrc;
use crate::metrics::{Metrics, inc};
use crate::util::{bad_json, esc, query};

use accounts::{AccountPool, Rotation};
use seek::Mp3Layout;
use session::{FORMAT_MP3_HIGH, FORMAT_MP3_LOW, MediaFormat, MediaLink, STREAM_CIPHER};
use stripe::StripeDecryptor;

pub use session::{MEDIA_URL_TTL, NO_ARL, REJECTED_MESSAGE};
pub use stripe::MASTER_KEY_SIZE;

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
/// Media URLs live this long in the cache (Cadence's `mediaURLTTL`).
const URL_ENTRIES: usize = 50_000;
pub(crate) const LANG: (&str, &str) = ("Accept-Language", "en-US,en;q=0.9");

/// The cover URL a track mix entry carries (`ALB_PICTURE`).
const COVER_PREFIX: &str = "https://cdn-images.dzcdn.net/images/cover/";
const COVER_SUFFIX: &str = "/1000x1000-000000-80-0-0.jpg";

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

/// Deezer endpoints, overridable in tests.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Public catalog API (default `https://api.deezer.com/2.0`).
    pub api: String,
    /// `gw-light.php` (default `https://www.deezer.com/ajax/gw-light.php`).
    pub gw: String,
    /// Media URL endpoint (default `https://media.deezer.com/v1/get_url`).
    pub media: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.deezer.com/2.0".into(),
            gw: "https://www.deezer.com/ajax/gw-light.php".into(),
            media: "https://media.deezer.com/v1/get_url".into(),
        }
    }
}

/// Streaming credentials, Cadence's `deezer.New` arguments.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// `DEEZER_ARL`, one or several account ARLs.
    pub arls: Vec<String>,
    /// `DEEZER_ARL_ROTATION` (`failover`, `round-robin`, `random`).
    pub rotation: String,
    /// `DEEZER_MASTER_KEY` (exactly [`MASTER_KEY_SIZE`] bytes). Unset = the
    /// source cannot decrypt, so it stays metadata-only.
    pub master_key: Option<String>,
    /// `DEEZER_HIGH_QUALITY` (default `true`): `MP3_128` before `MP3_64`.
    pub high_quality: bool,
}

/// Why a [`Deezer`] could not be built. Fatal at startup, as in Cadence.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{0}")]
    MasterKey(#[from] stripe::MasterKeySizeError),
    #[error("{0}")]
    Rotation(String),
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

/// One `song.getSearchTrackMix` entry (Cadence's `mixSong`).
#[derive(Debug, Default, Deserialize)]
struct MixSong {
    #[serde(default, rename = "SNG_ID")]
    id: String,
    #[serde(default, rename = "SNG_TITLE")]
    title: String,
    #[serde(default, rename = "VERSION")]
    version: String,
    #[serde(default, rename = "ART_NAME")]
    artist: String,
    #[serde(default, rename = "ALB_TITLE")]
    album: String,
    #[serde(default, rename = "ALB_PICTURE")]
    picture: String,
    #[serde(default, rename = "DURATION")]
    duration: String,
    #[serde(default, rename = "ISRC")]
    isrc: String,
}

impl MixSong {
    fn to_track(&self) -> Track {
        let mut title = self.title.clone();
        let version = self.version.trim();
        if !version.is_empty() && !title.to_lowercase().contains(&version.to_lowercase()) {
            title.push(' ');
            title.push_str(version);
        }
        let seconds: i64 = self.duration.parse().unwrap_or(0);
        let art = if self.picture.is_empty() {
            String::new()
        } else {
            format!("{COVER_PREFIX}{}{COVER_SUFFIX}", self.picture)
        };
        Track {
            source: NAME.into(),
            identifier: self.id.clone(),
            title,
            author: self.artist.clone(),
            album: self.album.clone(),
            duration_ms: seconds * 1000,
            isrc: self.isrc.clone(),
            artwork_url: art,
            uri: format!("https://www.deezer.com/track/{}", self.id),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct MixResults {
    #[serde(default)]
    data: Vec<MixSong>,
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
    let mut seen = HashSet::from([seed.to_string()]);
    tracks
        .into_iter()
        .filter(|t| seen.insert(t.identifier.clone()))
        .collect()
}

fn cap_tracks(mut tracks: Vec<Track>, limit: usize) -> Vec<Track> {
    tracks.truncate(limit);
    tracks
}

/// The Deezer source: catalog, streaming and recommendations.
pub struct Deezer {
    http: Arc<HttpClient>,
    endpoints: Endpoints,
    accounts: AccountPool,
    formats: Vec<MediaFormat>,
    decryptor: Option<StripeDecryptor>,
    urls: Lru<String, MediaLink>,
    isrc: Lru<String, Track>,
    radio: Lru<String, Vec<Track>>,
    layouts: Lru<String, Mp3Layout>,
    metrics: Arc<Metrics>,
}

impl Deezer {
    /// Build the source. A `master_key` of the wrong size (and an unknown
    /// rotation) fails the build, as in Cadence; an empty `master_key` or no
    /// ARL only disables streaming.
    pub fn new(
        http: Arc<HttpClient>,
        endpoints: Endpoints,
        setup: Setup,
        metrics: Arc<Metrics>,
    ) -> Result<Self, SetupError> {
        let rotation = Rotation::parse(&setup.rotation).map_err(SetupError::Rotation)?;
        let decryptor = match setup.master_key.filter(|k| !k.is_empty()) {
            Some(key) => Some(StripeDecryptor::new(&key)?),
            None => None,
        };
        let mut formats = vec![
            MediaFormat {
                cipher: STREAM_CIPHER.to_string(),
                format: FORMAT_MP3_HIGH.to_string(),
            },
            MediaFormat {
                cipher: STREAM_CIPHER.to_string(),
                format: FORMAT_MP3_LOW.to_string(),
            },
        ];
        if !setup.high_quality {
            formats.remove(0);
        }
        Ok(Self {
            http,
            endpoints,
            accounts: AccountPool::new(setup.arls, rotation),
            formats,
            decryptor,
            urls: Lru::new(URL_ENTRIES, MEDIA_URL_TTL),
            isrc: Lru::new(200_000, ISRC_CACHE_TTL),
            radio: Lru::new(RADIO_ENTRIES, RADIO_TTL),
            layouts: Self::new_layout_cache(),
            metrics,
        })
    }

    /// Whether full-length streams can be served (ARL + master key).
    fn streaming(&self) -> bool {
        self.decryptor.is_some() && !self.accounts.is_empty()
    }

    async fn api<T: serde::de::DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T, Failure> {
        let mut url = format!("{}{}", self.endpoints.api, path);
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

    /// The logged-in "track mix" of the seed track (Cadence's `trackMix`);
    /// empty when no ARL is configured.
    async fn track_mix(&self, seed: &str) -> Result<Vec<Track>, Failure> {
        if self.accounts.is_empty() {
            return Ok(vec![]);
        }
        self.first_account(|a| async move {
            let s = self.session(&a).await?;
            let (results, _) = self
                .gw(
                    &a,
                    Some(&s),
                    "song.getSearchTrackMix",
                    &json!({ "sng_id": seed, "start_with_input_track": false }),
                )
                .await?;
            let res: MixResults = serde_json::from_value(results).map_err(|e| bad_json(NAME, e))?;
            Ok(res
                .data
                .iter()
                .filter(|s| !s.id.is_empty() && !s.title.is_empty() && !s.artist.is_empty())
                .map(MixSong::to_track)
                .collect())
        })
        .await
    }

    /// The public artist radio of the seed track (Cadence's `artistRadio`).
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

    /// Stream `track` from the first byte (Cadence's `Open`, minus the head
    /// and disk cache which the resolver generalizes).
    async fn open_stream(&self, t: &Track, dec: &StripeDecryptor) -> Result<Stream, Failure> {
        let u = self.media_url(&t.identifier).await?;
        let req = Req::get(&u.url).map_err(|e| e.to_failure(NAME))?.header(LANG.0, LANG.1);
        let res = match self.http.stream(&req).await {
            Ok(r) => r,
            Err(e) => {
                self.urls.delete(&t.identifier);
                return Err(e.to_failure(NAME));
            }
        };
        let body = crate::util::response_body(res);
        Ok(Stream {
            body: Box::new(dec.decrypt(&t.identifier, body)),
            format: Format::mp3(),
            start_sample: 0,
            live: false,
        })
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

    /// Streaming, when an ARL and a master key are configured; otherwise the
    /// resolver mirrors Deezer tracks like Spotify's.
    fn playable(&self) -> Option<&dyn Playable> {
        self.streaming().then_some(self)
    }

    fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
        Some(self)
    }

    fn recommender(&self) -> Option<&dyn Recommender> {
        Some(self)
    }
}

#[async_trait]
impl Playable for Deezer {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let Some(dec) = &self.decryptor else {
            return Err(seek::no_decryptor());
        };
        if start_ms >= seek::SEEK_MIN_MS {
            match self.seek_open(track, start_ms, dec).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    inc(&self.metrics.seek.fallback);
                    tracing::debug!(track = %track.key(), error = %e, "range seek unavailable, streaming from the start");
                }
            }
        }
        self.open_stream(track, dec).await
    }

    fn seekable(&self) -> bool {
        self.decryptor.is_some()
    }

    /// Resolve the stream URL early; the resolver does the head prefetch.
    async fn warm(&self, track: &Track) {
        if self.streaming() {
            if let Err(e) = self.media_url(&track.identifier).await {
                tracing::debug!(track = %track.key(), error = %e, "warm failed");
            }
        }
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
    /// The track mix of the seed track, falling back to its artist radio.
    /// `identifier` is a Deezer track id or `isrc:<ISRC>`.
    async fn recommend(&self, identifier: &str, limit: usize) -> Result<Vec<Track>, Failure> {
        let limit = limit.clamp(1, RADIO_MAX_LIMIT);
        let seed = self.seed_id(identifier.trim()).await?;
        if let Some(hit) = self.radio.get(&seed) {
            return Ok(cap_tracks(hit, limit));
        }
        let mut origin = &self.metrics.deezer_recommend.track_mix;
        let mut tracks = match self.track_mix(&seed).await {
            Ok(t) if !t.is_empty() => t,
            mix => {
                if let Err(e) = &mix {
                    tracing::warn!(track = %seed, error = %e, "track mix failed, using artist radio");
                }
                origin = &self.metrics.deezer_recommend.artist_radio;
                self.artist_radio(&seed).await?
            }
        };
        tracks = without_seed(tracks, &seed);
        if tracks.is_empty() {
            return Ok(vec![]);
        }
        inc(origin);
        self.radio.set(seed, tracks.clone());
        Ok(cap_tracks(tracks, limit))
    }
}

/// Fakes shared by the tests below and by [`session`]'s tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Path, Query};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Redirect, Response};
    use axum::routing::{get, post};
    use parking_lot::RwLock;
    use serde_json::{Value, json};

    use super::*;
    use crate::httpx::{HttpClient, HttpOptions};
    use crate::testutil::serve_with;

    /// The master key every test decrypts with.
    pub const TEST_MASTER: &str = "0123456789abcdef";

    /// One recorded `gw-light.php` call.
    #[derive(Debug, Clone)]
    pub struct GwCall {
        pub method: String,
        pub cookie: String,
    }

    #[derive(Default)]
    pub struct FakeState {
        reject_accounts: AtomicBool,
        expire_once: AtomicBool,
        drop_track_token: AtomicBool,
        refuse_media: AtomicBool,
        refuse_mix: AtomicBool,
        ignore_range: AtomicBool,
        media_calls: AtomicU32,
        gw_calls: Mutex<Vec<GwCall>>,
        ranges: Mutex<Vec<String>>,
        content: RwLock<Vec<u8>>,
    }

    /// A fake Deezer: public API, `gw-light.php`, media endpoint and a
    /// Range-capable CDN.
    pub struct Fake {
        state: Arc<FakeState>,
        base: String,
    }

    impl Fake {
        pub fn base(&self) -> &str {
            &self.base
        }

        /// A Deezer wired to this fake, with two ARLs and the test key.
        pub fn deezer(&self) -> Deezer {
            self.build(vec!["arl-one".into(), "arl-two".into()])
        }

        /// Like [`Fake::deezer`] with explicit ARLs.
        pub fn build(&self, arls: Vec<String>) -> Deezer {
            self.build_setup(Setup {
                arls,
                rotation: "failover".into(),
                master_key: Some(TEST_MASTER.into()),
                high_quality: true,
            })
        }

        /// Like [`Fake::build`] with full control over the credentials.
        pub fn build_setup(&self, setup: Setup) -> Deezer {
            Deezer::new(
                client(),
                Endpoints {
                    api: format!("{}/api", self.base),
                    gw: format!("{}/gw-light.php", self.base),
                    media: format!("{}/media/get_url", self.base),
                },
                setup,
                Arc::new(Metrics::default()),
            )
            .unwrap()
        }

        pub fn gw_calls(&self) -> Vec<GwCall> {
            self.state.gw_calls.lock().unwrap().clone()
        }

        pub fn media_calls(&self) -> u32 {
            self.state.media_calls.load(Ordering::SeqCst)
        }

        pub fn ranges(&self) -> Vec<String> {
            self.state.ranges.lock().unwrap().clone()
        }

        pub fn set_content(&self, bytes: Vec<u8>) {
            *self.state.content.write() = bytes;
        }

        pub fn reject_accounts(&self, on: bool) {
            self.state.reject_accounts.store(on, Ordering::SeqCst);
        }
        /// The next `song.getListData` answers `VALID_TOKEN_REQUIRED`.
        pub fn expire_once(&self) {
            self.state.expire_once.store(true, Ordering::SeqCst);
        }
        pub fn drop_track_token(&self, on: bool) {
            self.state.drop_track_token.store(on, Ordering::SeqCst);
        }
        pub fn refuse_media(&self, on: bool) {
            self.state.refuse_media.store(on, Ordering::SeqCst);
        }
        pub fn refuse_mix(&self, on: bool) {
            self.state.refuse_mix.store(on, Ordering::SeqCst);
        }
        pub fn ignore_range(&self, on: bool) {
            self.state.ignore_range.store(on, Ordering::SeqCst);
        }
    }

    pub fn client() -> Arc<HttpClient> {
        Arc::new(HttpClient::new(NAME, HttpOptions::default()).unwrap())
    }

    /// A Deezer with accounts but no live endpoints: for account-walk tests.
    pub fn bare_deezer(arls: &[&str], rotation: Rotation) -> Deezer {
        Deezer::new(
            client(),
            Endpoints::default(),
            Setup {
                arls: arls.iter().map(|a| a.to_string()).collect(),
                rotation: match rotation {
                    Rotation::Failover => "failover",
                    Rotation::RoundRobin => "round-robin",
                    Rotation::Random => "random",
                }
                .into(),
                master_key: Some(TEST_MASTER.into()),
                high_quality: true,
            },
            Arc::new(Metrics::default()),
        )
        .unwrap()
    }

    fn t(id: i64, title: &str) -> Value {
        json!({"id": id, "title": title, "duration": 200, "isrc": "GBAAA0000001", "link": format!("https://www.deezer.com/track/{id}"),
               "artist": {"id": 27, "name": "Daft Punk"}, "album": {"title": "Discovery", "cover_xl": "https://cover/xl.jpg"}})
    }

    /// A synthetic CBR (or Xing-VBR-marked) MPEG-1 Layer III stream, as in
    /// Cadence's `synthMP3`.
    pub fn synth_mp3(frames: usize, vbr: bool) -> Vec<u8> {
        const BITRATE: i64 = 128;
        const RATE: i64 = 44100;
        let mut b = vec![b'I', b'D', b'3', 3, 0, 0, 0, 0, 0, 100];
        b.extend(std::iter::repeat_n(0u8, 100));
        let exact = 144.0 * BITRATE as f64 * 1000.0 / RATE as f64;
        for i in 0..frames {
            let size = ((i + 1) as f64 * exact).floor() as usize - (i as f64 * exact).floor() as usize;
            let pad = u8::from(size > 417);
            let mut f = vec![0u8; size];
            f[0] = 0xFF;
            f[1] = 0xFB;
            f[2] = 0x90 | (pad << 1);
            f[3] = 0x00;
            if i == 0 {
                let tag: &[u8] = if vbr { b"Xing" } else { b"Info" };
                f[36..36 + tag.len()].copy_from_slice(tag);
            }
            b.extend(f);
        }
        b
    }

    /// Byte offset of `frame` in [`synth_mp3`] (same arithmetic as
    /// `Mp3Layout::offset`).
    pub fn synth_offset(frame: i64) -> usize {
        110 + (frame as f64 * (144.0 * 128.0 * 1000.0 / 44100.0)).floor() as usize
    }

    /// Start the fake and wire a [`Fake`] handle to it.
    pub async fn fake_deezer() -> Fake {
        let state = Arc::new(FakeState::default());
        let base = serve_with({
            let state = state.clone();
            move |base| catalog().merge(gateway(state, base))
        })
        .await;
        Fake { state, base }
    }

    /// The public catalog routes (under `/api`).
    fn catalog() -> Router {
        Router::new()
            .route(
                "/api/search",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    axum::Json(json!({"data": [t(1, &q["q"]), {"id": 2, "title": "gone", "readable": false}]}))
                }),
            )
            .route(
                "/api/track/{id}",
                get(|Path(id): Path<String>| async move {
                    match id.as_str() {
                        "isrc:GBAAA0000001" | "3135553" => axum::Json(t(3135553, "One More Time")).into_response(),
                        _ => axum::Json(json!({"error": {"type": "DataException", "message": "no data", "code": 800}})).into_response(),
                    }
                }),
            )
            .route("/api/album/{id}", get(|| async { axum::Json(json!({"title": "Discovery", "link": "l", "cover_xl": "c", "tracks": {"data": [{"id": 5, "title": "A", "artist": {"name": "x"}}]}})) }))
            .route("/api/playlist/{id}", get(|| async { axum::Json(json!({"title": "PL", "nb_tracks": 3, "picture_xl": "p", "link": "l"})) }))
            .route(
                "/api/playlist/{id}/tracks",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q["index"] == "0" { axum::Json(json!({"data": [t(10, "a"), t(11, "b")], "next": "x"})) } else { axum::Json(json!({"data": [t(12, "c")]})) }
                }),
            )
            .route("/api/artist/{id}", get(|| async { axum::Json(json!({"name": "Daft Punk", "picture_xl": "p", "link": "l"})) }))
            .route("/api/artist/{id}/top", get(|| async { axum::Json(json!({"data": [t(20, "top")]})) }))
            .route("/api/artist/27/radio", get(|| async { axum::Json(json!({"data": [t(3135553, "seed"), t(7, "x"), t(7, "x"), t(8, "y"), t(9, "z")]})) }))
            .route("/api/short", get(|| async { Redirect::temporary("https://www.deezer.com/fr/track/3135553?utm=1") }))
    }

    /// `gw-light.php`, the media endpoint and the CDN.
    fn gateway(state: Arc<FakeState>, base: String) -> Router {
        let cdn_url = format!("{base}/cdn/track.mp3");
        let gw_state = state.clone();
        let media_state = state.clone();
        Router::new()
            .route(
                "/gw-light.php",
                post(move |Query(q): Query<HashMap<String, String>>, headers: HeaderMap| {
                    let state = gw_state.clone();
                    async move {
                        let cookie = headers
                            .get(header::COOKIE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let method = q.get("method").cloned().unwrap_or_default();
                        state.gw_calls.lock().unwrap().push(GwCall {
                            method: method.clone(),
                            cookie,
                        });
                        let results = match method.as_str() {
                            "deezer.getUserData" => {
                                if state.reject_accounts.load(Ordering::SeqCst) {
                                    json!({})
                                } else {
                                    json!({"checkForm": "check-form", "USER": {"USER_ID": 1, "OPTIONS": {"license_token": "license-1"}}})
                                }
                            }
                            "song.getListData" => {
                                if state.expire_once.swap(false, Ordering::SeqCst) {
                                    return axum::Json(json!({"error": {"code": 400, "message": "VALID_TOKEN_REQUIRED"}, "results": {}}))
                                        .into_response();
                                }
                                if state.drop_track_token.load(Ordering::SeqCst) {
                                    json!({"data": []})
                                } else {
                                    json!({"data": [{"TRACK_TOKEN": "token-1"}]})
                                }
                            }
                            "song.getSearchTrackMix" => {
                                if state.refuse_mix.load(Ordering::SeqCst) {
                                    return axum::Json(json!({"error": {"code": 400, "message": "nope"}, "results": {}})).into_response();
                                }
                                json!({"data": [
                                    {"SNG_ID": "3135553", "SNG_TITLE": "One More Time", "VERSION": "", "ART_NAME": "Daft Punk", "ALB_TITLE": "Discovery", "ALB_PICTURE": "abc", "DURATION": "200", "ISRC": "GBAAA0000001"},
                                    {"SNG_ID": "7", "SNG_TITLE": "Sweater Weather", "VERSION": "(Remastered)", "ART_NAME": "The Neighbourhood", "ALB_TITLE": "I Love You.", "ALB_PICTURE": "def", "DURATION": "240", "ISRC": "USSM11204960"}
                                ]})
                            }
                            _ => json!({}),
                        };
                        let mut res = axum::Json(json!({"error": [], "results": results})).into_response();
                        if method == "deezer.getUserData" && !state.reject_accounts.load(Ordering::SeqCst) {
                            res.headers_mut().insert(header::SET_COOKIE, "sid=sid-1; Path=/".parse().unwrap());
                        }
                        res
                    }
                }),
            )
            .route(
                "/media/get_url",
                post(move |_body: String| {
                    let state = media_state.clone();
                    let url = cdn_url.clone();
                    async move {
                        state.media_calls.fetch_add(1, Ordering::SeqCst);
                        if state.refuse_media.load(Ordering::SeqCst) {
                            return axum::Json(
                                json!({"data": [{"media": [], "errors": [{"code": 2002, "message": "FALLBACK"}]}]}),
                            )
                            .into_response();
                        }
                        axum::Json(
                            json!({"data": [{"media": [{"format": "MP3_128", "sources": [{"url": url}]}], "errors": []}]}),
                        )
                        .into_response()
                    }
                }),
            )
            .route(
                "/cdn/track.mp3",
                get(move |headers: HeaderMap| {
                    let state = state.clone();
                    async move { cdn(&state, &headers) }
                }),
            )
    }

    /// Serve the (encrypted) test content, honouring `Range` requests.
    fn cdn(state: &FakeState, headers: &HeaderMap) -> Response {
        let data = state.content.read().clone();
        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        state.ranges.lock().unwrap().push(range.clone());
        let (status, start, end) = if state.ignore_range.load(Ordering::SeqCst) || range.is_empty() {
            (StatusCode::OK, 0, data.len())
        } else {
            let r = range.trim_start_matches("bytes=");
            let (a, b) = r.split_once('-').unwrap_or(("0", ""));
            let start: usize = a.parse().unwrap_or(0).min(data.len());
            let end: usize = if b.is_empty() {
                data.len()
            } else {
                (b.parse::<usize>().unwrap_or(data.len()) + 1).min(data.len())
            };
            (StatusCode::PARTIAL_CONTENT, start, end.max(start))
        };
        let mut res = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "audio/mpeg");
        if status == StatusCode::PARTIAL_CONTENT {
            res = res.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, end.saturating_sub(1), data.len()),
            );
        }
        res.body(Body::from(data[start..end].to_vec())).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use wavyzon_plugin::{LoadType, Severity};

    #[tokio::test]
    async fn search_tracks_and_isrc() {
        let fake = fake_deezer().await;
        let dz = fake.deezer();
        let got = dz.search("daft").await.unwrap();
        assert_eq!(got.len(), 1, "unreadable tracks are dropped");
        assert_eq!(got[0].duration_ms, 200_000);
        assert_eq!(got[0].artwork_url, "https://cover/xl.jpg");
        assert_eq!(dz.by_isrc("gb-aaa-00-00001").await.unwrap().identifier, "3135553");
        dz.by_isrc("GBAAA0000001").await.unwrap();
        assert_eq!(fake.gw_calls().len(), 0, "the catalog never logs in");
        assert_eq!(dz.search("dzisrc:GBAAA0000001").await.unwrap()[0].identifier, "3135553");
        let err = dz.track("404").await.unwrap_err();
        assert_eq!(err.severity, Severity::Common);
    }

    #[tokio::test]
    async fn links_resolve() {
        let fake = fake_deezer().await;
        let dz = fake.deezer();
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
            dz.expand(&format!("{}/api/short", fake.base())).await.unwrap(),
            "https://www.deezer.com/fr/track/3135553"
        );
    }

    #[tokio::test]
    async fn recommend_prefers_the_track_mix() {
        let fake = fake_deezer().await;
        let dz = fake.deezer();
        let got = dz.recommend("3135553", 2).await.unwrap();
        let ids: Vec<&str> = got.iter().map(|t| t.identifier.as_str()).collect();
        assert_eq!(ids, ["7"], "the seed is dropped, the mix wins");
        assert_eq!(got[0].title, "Sweater Weather (Remastered)");
        assert_eq!(got[0].artwork_url, format!("{COVER_PREFIX}def{COVER_SUFFIX}"));
        assert_eq!(got[0].duration_ms, 240_000);
    }

    #[tokio::test]
    async fn recommend_falls_back_to_the_artist_radio() {
        let fake = fake_deezer().await;
        fake.refuse_mix(true);
        let dz = fake.deezer();
        let got = dz.recommend("3135553", 2).await.unwrap();
        let ids: Vec<&str> = got.iter().map(|t| t.identifier.as_str()).collect();
        assert_eq!(ids, ["7", "8"], "artist radio answers when the mix refuses");
    }

    #[tokio::test]
    async fn recommend_without_an_account_uses_the_artist_radio() {
        let dz = bare_deezer(&[], accounts::Rotation::Failover);
        dz.radio.set(
            "42".into(),
            vec![Track {
                identifier: "7".into(),
                ..Default::default()
            }],
        );
        let got = dz.recommend("42", 1).await.unwrap();
        assert_eq!(got.len(), 1);
    }

    #[tokio::test]
    async fn recommend_rejects_a_seed_that_is_not_a_deezer_id() {
        let dz = bare_deezer(&[], accounts::Rotation::Failover);
        assert!(dz.recommend("soundcloud-track", 10).await.is_err());
    }

    fn dz_track() -> Track {
        Track {
            source: NAME.into(),
            identifier: "3135553".into(),
            ..Default::default()
        }
    }

    async fn read_all(s: &mut Stream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut got = Vec::new();
        s.body.read_to_end(&mut got).await.unwrap();
        got
    }

    fn encrypted(plain: &[u8]) -> Vec<u8> {
        stripe::stripe_encrypt(&StripeDecryptor::new(TEST_MASTER).unwrap(), "3135553", plain)
    }

    #[tokio::test]
    async fn open_decrypts_the_stream() {
        let fake = fake_deezer().await;
        let plain: Vec<u8> = (0..9000).map(|i| (i * 31 + i / 7) as u8).collect();
        fake.set_content(encrypted(&plain));
        let dz = fake.deezer();
        let mut s = dz.open(&dz_track(), 0).await.unwrap();
        assert_eq!(s.format, Format::mp3());
        assert_eq!(s.start_sample, 0);
        let got = read_all(&mut s).await;
        let diff = got.iter().zip(&plain).position(|(a, b)| a != b);
        assert_eq!(got.len(), plain.len(), "lengths differ, first diff at {diff:?}");
        assert_eq!(
            diff,
            None,
            "first diff at {diff:?}: {:?} vs {:?}",
            got.get(diff.unwrap_or(0)),
            plain.get(diff.unwrap_or(0))
        );
    }

    #[tokio::test]
    async fn without_credentials_deezer_is_metadata_only() {
        let fake = fake_deezer().await;
        let dz = fake.build_setup(Setup::default());
        assert!(dz.playable().is_none(), "no ARL, no master key: no playback");
        let err = dz.open(&dz_track(), 0).await.err().unwrap();
        assert_eq!(err.severity, Severity::Fault);
    }

    #[tokio::test]
    async fn range_seek_lands_on_the_exact_frame() {
        let fake = fake_deezer().await;
        let plain = synth_mp3(3000, false);
        fake.set_content(encrypted(&plain));
        let dz = fake.deezer();
        for ms in [15_000i64, 42_345, 60_000] {
            let mut s = dz.open(&dz_track(), ms).await.unwrap();
            let first = ms * 44100 / 1000 / 1152 - 4;
            assert_eq!(s.start_sample, first * 1152 * 48_000 / 44100, "{ms}");
            assert_eq!(read_all(&mut s).await, plain[synth_offset(first)..], "{ms}");
        }
        let ranges = fake.ranges();
        assert!(
            ranges.iter().any(|r| r.starts_with("bytes=") && r != "bytes=0-"),
            "a seek must fetch a byte range, not the whole file: {ranges:?}"
        );
    }

    #[tokio::test]
    async fn a_cdn_that_ignores_ranges_still_seeks() {
        use std::sync::atomic::Ordering;
        let fake = fake_deezer().await;
        let plain = synth_mp3(3000, false);
        fake.set_content(encrypted(&plain));
        fake.ignore_range(true);
        let dz = fake.deezer();
        let ms = 15_000i64;
        let mut s = dz.open(&dz_track(), ms).await.unwrap();
        let first = ms * 44100 / 1000 / 1152 - 4;
        assert_eq!(read_all(&mut s).await, plain[synth_offset(first)..]);
        // The probe and the seek each get the whole file back.
        assert_eq!(dz.metrics.cdn_range_ignored.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn seeking_a_vbr_stream_falls_back_to_the_whole_file() {
        use std::sync::atomic::Ordering;
        let fake = fake_deezer().await;
        let plain = synth_mp3(2000, true);
        fake.set_content(encrypted(&plain));
        let dz = fake.deezer();
        let mut s = dz.open(&dz_track(), 20_000).await.unwrap();
        assert_eq!(s.start_sample, 0, "the player skips forward from the start");
        assert_eq!(read_all(&mut s).await, plain);
        assert_eq!(dz.metrics.seek.fallback.load(Ordering::SeqCst), 1);
        assert_eq!(dz.metrics.seek.range.load(Ordering::SeqCst), 0);
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
