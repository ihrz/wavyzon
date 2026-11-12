//! # wavyzon-plugin-genius
//!
//! [Genius](https://genius.com) lyrics for `/v1/lyrics` and `/v4/lyrics`
//! (LavaLyrics' API). The node asks this provider first and its built-in
//! lyrics.ovh client when Genius has nothing or fails:
//!
//! ```text
//! Genius on:  Genius ──none / error──► lyrics.ovh
//! Genius off: lyrics.ovh
//! ```
//!
//! A lookup searches Genius for the track's cleaned title and first artist,
//! keeps the first hit with the same title and artist (strict: a wrong song's
//! lyrics are worse than lyrics.ovh's), and reads the lyrics from the song
//! page (`data-lyrics-container` blocks; Genius' API has no lyrics endpoint).
//! Hits and misses are cached, failures are not.
//!
//! Search goes through the official API when a key is set, else through
//! genius.com's public web search (no key; works, but is what Genius
//! rate-limits first):
//!
//! 1. `ACCESS_TOKEN`: a Client Access Token from <https://genius.com/api-clients>
//!    (the reliable option).
//! 2. `CLIENT_ID` + `CLIENT_SECRET`: exchanged for a token at
//!    `POST /oauth/token` (`grant_type=client_credentials`). If Genius refuses
//!    the exchange, the plugin logs it, uses the public search and retries the
//!    exchange 10 minutes later.
//! 3. Neither: the public search.
//!
//! A token Genius rejects (401) is logged and the public search is used.
//!
//! On by default (cargo feature `plugin-genius`). Settings, all optional,
//! from `WAVYZON_PLUGIN_GENIUS_<KEY>`:
//!
//! | Variable | Default | Meaning |
//! | --- | --- | --- |
//! | `ENABLED` | `true` | `false` registers nothing (lyrics.ovh only) |
//! | `ACCESS_TOKEN` | none | Client Access Token |
//! | `CLIENT_ID` | none | API client id (with `CLIENT_SECRET`) |
//! | `CLIENT_SECRET` | none | API client secret (with `CLIENT_ID`) |
//! | `TIMEOUT_MS` | `10000` | Whole Genius lookup (search + page); lyrics.ovh is asked after it |
//! | `PROXY` | none | `http://`, `https://` or `socks5://` proxy |
//! | `API_URL` | `https://api.genius.com` | API base (mirror, tests) |
//! | `WEB_URL` | `https://genius.com` | Site base: public search and song pages |

mod html;
mod pick;

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde::Deserialize;
use url::Url;
use wavyzon_plugin::{
    API_VERSION, Failure, Lyrics, LyricsProvider, Plugin, PluginContext, PluginError, PluginManifest, Registry, Track,
    async_trait,
};
use wavyzon_sources::cache::Lru;
use wavyzon_sources::httpx::{HttpClient, HttpError, HttpOptions, Req};
use wavyzon_sources::lyrics::{clean_artist, clean_title};

pub use html::extract_lyrics;
pub use pick::{Artist, Hit, pick, query, words};

/// Plugin name: `WAVYZON_PLUGINS` entry and `WAVYZON_PLUGIN_GENIUS_*` settings.
pub const NAME: &str = "genius";
/// What the lyrics API reports as the provider.
pub const PROVIDER: &str = "genius.com";
pub const DEFAULT_API_URL: &str = "https://api.genius.com";
pub const DEFAULT_WEB_URL: &str = "https://genius.com";
/// Largest song page read (they are a few hundred KB).
pub const MAX_PAGE_BYTES: usize = 4 << 20;
/// Default bound of a whole lookup.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

const ENTRIES: usize = 20_000;
/// Lyrics of a song do not change.
const FOUND_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// A song may get a Genius page later.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
/// Wait after a refused client-credentials exchange before trying again.
const TOKEN_RETRY: Duration = Duration::from_secs(600);
/// Refresh a token this long before Genius says it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// Plugin settings (see the crate docs).
#[derive(Clone)]
pub struct Settings {
    pub enabled: bool,
    pub access_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub api_url: Url,
    pub web_url: Url,
    pub timeout: Duration,
    pub proxy: Option<String>,
}

impl std::fmt::Debug for Settings {
    /// Never prints the secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("enabled", &self.enabled)
            .field("access_token", &self.access_token.as_ref().map(|_| "<set>"))
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<set>"))
            .field("api_url", &self.api_url.as_str())
            .field("web_url", &self.web_url.as_str())
            .field("timeout", &self.timeout)
            .field("proxy", &self.proxy.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            access_token: None,
            client_id: None,
            client_secret: None,
            api_url: Url::parse(DEFAULT_API_URL).expect("default url parses"),
            web_url: Url::parse(DEFAULT_WEB_URL).expect("default url parses"),
            timeout: DEFAULT_TIMEOUT,
            proxy: None,
        }
    }
}

impl Settings {
    /// Read `WAVYZON_PLUGIN_GENIUS_*`. Flags use the node's three-way rule
    /// (`1/true/yes/on`, `0/false/no/off`, anything else keeps the default);
    /// other bad values stop the node with a message.
    pub fn from_context(ctx: &PluginContext) -> Result<Self, PluginError> {
        Self::from_lookup(|k| ctx.setting(k).map(str::to_string))
    }

    /// Like [`Settings::from_context`], from any lookup of the lowercase keys.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, PluginError> {
        let get = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let bad = |k: &str, msg: String| PluginError::Config(format!("WAVYZON_PLUGIN_GENIUS_{k}: {msg}"));
        let mut s = Self {
            enabled: !matches!(
                get("enabled").map(|v| v.to_ascii_lowercase()).as_deref(),
                Some("0" | "false" | "no" | "off")
            ),
            access_token: get("access_token"),
            client_id: get("client_id"),
            client_secret: get("client_secret"),
            ..Self::default()
        };
        match (&s.client_id, &s.client_secret) {
            (Some(_), None) => return Err(bad("CLIENT_SECRET", "required with CLIENT_ID".into())),
            (None, Some(_)) => return Err(bad("CLIENT_ID", "required with CLIENT_SECRET".into())),
            _ => {}
        }
        let url = |k: &str, d: &Url| -> Result<Url, PluginError> {
            let Some(v) = get(k) else { return Ok(d.clone()) };
            let u = Url::parse(&v).map_err(|e| bad(&k.to_ascii_uppercase(), e.to_string()))?;
            if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
                return Err(bad(&k.to_ascii_uppercase(), "must be an http(s) URL".into()));
            }
            Ok(u)
        };
        s.api_url = url("api_url", &s.api_url)?;
        s.web_url = url("web_url", &s.web_url)?;
        if let Some(v) = get("timeout_ms") {
            let ms: u64 = v
                .parse()
                .ok()
                .filter(|&ms| ms > 0)
                .ok_or_else(|| bad("TIMEOUT_MS", format!("{v:?} is not a positive number")))?;
            s.timeout = Duration::from_millis(ms);
        }
        s.proxy = get("proxy");
        Ok(s)
    }

    /// How searches are authenticated, for the startup log.
    pub fn auth_mode(&self) -> &'static str {
        if self.access_token.is_some() {
            "access token"
        } else if self.client_id.is_some() {
            "client credentials"
        } else {
            "public search (no key)"
        }
    }
}

/// A failed request as a [`Failure`], without the response body: Genius'
/// refusals are full HTML pages (an anti-bot page on a 403).
fn fail(e: HttpError) -> Failure {
    match e {
        HttpError::Status { status, url, .. } => Failure::from_status(NAME, status, format!("{url} returned {status}")),
        e => e.to_failure(NAME),
    }
}

/// `base` + `path` (`/search`), keeping any path prefix of `base`.
fn join(base: &Url, path: &str) -> Result<Url, Failure> {
    let raw = format!("{}{}", base.as_str().trim_end_matches('/'), path);
    Url::parse(&raw).map_err(|e| Failure::common(NAME, "invalid Genius URL", Some(e.to_string())))
}

#[derive(Default)]
struct TokenState {
    /// The token and when to stop using it.
    token: Option<(String, Option<Instant>)>,
    /// Last refused exchange.
    failed_at: Option<Instant>,
}

enum Auth {
    None,
    Token(String),
    Client {
        id: String,
        secret: String,
        state: tokio::sync::Mutex<TokenState>,
    },
}

/// The Genius lyrics provider with its caches.
pub struct GeniusClient {
    settings: Settings,
    http: HttpClient,
    auth: Auth,
    found: Lru<String, Lyrics>,
    missing: Lru<String, ()>,
}

/// Search answers: the API's `response.hits`, the web search's
/// `response.sections[].hits`.
#[derive(Deserialize, Default)]
struct SearchAnswer {
    #[serde(default)]
    response: SearchResponse,
}

#[derive(Deserialize, Default)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<SearchHit>,
    #[serde(default)]
    sections: Vec<Section>,
}

#[derive(Deserialize)]
struct Section {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    hits: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct SearchHit {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    result: Option<Hit>,
}

impl SearchAnswer {
    fn songs(self) -> Vec<Hit> {
        let song = |k: &Option<String>| k.as_deref().is_none_or(|k| k == "song");
        let r = self.response;
        r.hits
            .into_iter()
            .chain(r.sections.into_iter().filter(|s| song(&s.kind)).flat_map(|s| s.hits))
            .filter(|h| song(&h.kind))
            .filter_map(|h| h.result)
            .collect()
    }
}

impl GeniusClient {
    pub fn new(settings: Settings) -> Result<Self, PluginError> {
        let t = settings.timeout;
        let opts = HttpOptions {
            proxy: settings.proxy.clone(),
            connect_timeout: t.min(Duration::from_secs(5)),
            request_timeout: t,
            header_timeout: t,
            // The URLs are the operator's settings, never a client's.
            block_private: false,
            ..HttpOptions::default()
        };
        let http = HttpClient::new(NAME, opts).map_err(|e| PluginError::Config(e.to_string()))?;
        let auth = match (&settings.access_token, &settings.client_id, &settings.client_secret) {
            (Some(t), _, _) => Auth::Token(t.clone()),
            (None, Some(id), Some(secret)) => Auth::Client {
                id: id.clone(),
                secret: secret.clone(),
                state: tokio::sync::Mutex::new(TokenState::default()),
            },
            _ => Auth::None,
        };
        Ok(Self {
            settings,
            http,
            auth,
            found: Lru::new(ENTRIES, FOUND_TTL),
            missing: Lru::new(ENTRIES, MISSING_TTL),
        })
    }

    /// The API token to search with, if any (exchanging the client
    /// credentials when needed).
    async fn token(&self) -> Option<String> {
        let (id, secret, state) = match &self.auth {
            Auth::None => return None,
            Auth::Token(t) => return Some(t.clone()),
            Auth::Client { id, secret, state } => (id, secret, state),
        };
        // One exchange at a time; the others wait for its token.
        let mut st = state.lock().await;
        if let Some((t, until)) = &st.token
            && until.is_none_or(|u| Instant::now() < u)
        {
            return Some(t.clone());
        }
        if st.failed_at.is_some_and(|f| f.elapsed() < TOKEN_RETRY) {
            return None;
        }
        match self.exchange(id, secret).await {
            Ok((token, ttl)) => {
                tracing::info!(plugin = NAME, "Genius client credentials exchanged for a token");
                let until = ttl.map(|d| Instant::now() + d.saturating_sub(TOKEN_MARGIN));
                st.token = Some((token.clone(), until));
                st.failed_at = None;
                Some(token)
            }
            Err(e) => {
                tracing::warn!(
                    plugin = NAME,
                    error = %e,
                    "Genius refused the client credentials; using the public search (set WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN to a Client Access Token instead)"
                );
                st.token = None;
                st.failed_at = Some(Instant::now());
                None
            }
        }
    }

    /// `POST /oauth/token` with `grant_type=client_credentials`.
    async fn exchange(&self, id: &str, secret: &str) -> Result<(String, Option<Duration>), Failure> {
        let url = join(&self.settings.api_url, "/oauth/token")?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", id)
            .append_pair("client_secret", secret)
            .append_pair("grant_type", "client_credentials")
            .finish();
        let req = Req::new(Method::POST, url.as_str())
            .map_err(|e| e.to_failure(NAME))?
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body);
        let bytes = self.http.fetch(&req).await.map_err(fail)?;
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            access_token: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
        }
        let a: Answer = serde_json::from_slice(&bytes)
            .map_err(|e| Failure::common(NAME, "bad Genius token answer", Some(e.to_string())))?;
        match a.access_token.filter(|t| !t.is_empty()) {
            Some(t) => Ok((t, a.expires_in.map(Duration::from_secs))),
            None => Err(Failure::common(NAME, "Genius answered without an access token", None)),
        }
    }

    /// Forget an exchanged token Genius rejected; a fixed token cannot be
    /// renewed, so it is only logged.
    async fn rejected(&self) {
        match &self.auth {
            Auth::Client { state, .. } => {
                let mut st = state.lock().await;
                st.token = None;
                tracing::warn!(plugin = NAME, "Genius rejected the exchanged token; it will be renewed");
            }
            Auth::Token(_) => tracing::warn!(
                plugin = NAME,
                "Genius rejected WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN; using the public search"
            ),
            Auth::None => {}
        }
    }

    async fn get_json(&self, url: Url, token: Option<&str>) -> Result<Vec<Hit>, HttpError> {
        let mut req = Req::get(url.as_str())?.header("Accept", "application/json");
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let bytes = self.http.fetch(&req).await?;
        let answer: SearchAnswer = serde_json::from_slice(&bytes).map_err(|e| HttpError::Transport {
            url: wavyzon_sources::httpx::safe_url(&url),
            message: format!("bad JSON: {e}"),
        })?;
        Ok(answer.songs())
    }

    /// Song hits for `q`: the API with a token, else (or when the API
    /// fails) the public web search.
    async fn search(&self, q: &str) -> Result<Vec<Hit>, Failure> {
        if let Some(token) = self.token().await {
            let mut url = join(&self.settings.api_url, "/search")?;
            url.query_pairs_mut().append_pair("q", q);
            match self.get_json(url, Some(&token)).await {
                Ok(hits) => return Ok(hits),
                Err(e) if e.status() == Some(401) => self.rejected().await,
                Err(e) => {
                    tracing::warn!(plugin = NAME, error = %fail(e), "Genius API search failed, trying the public search")
                }
            }
        }
        let mut url = join(&self.settings.web_url, "/api/search/song")?;
        url.query_pairs_mut().append_pair("q", q).append_pair("per_page", "10");
        self.get_json(url, None).await.map_err(fail)
    }

    /// The song page at `path` (`/Artist-title-lyrics`), `None` on a 404.
    async fn page(&self, path: &str) -> Result<Option<String>, Failure> {
        let url = join(&self.settings.web_url, path)?;
        // `path` comes from Genius' answer: stay on the configured site.
        if url.host_str() != self.settings.web_url.host_str() || url.port() != self.settings.web_url.port() {
            return Err(Failure::suspicious(NAME, "Genius song path leaves the site", None));
        }
        let req = Req::get(url.as_str())
            .map_err(|e| e.to_failure(NAME))?
            .header("Accept", "text/html");
        let mut res = self
            .http
            .execute(&req, Some(self.settings.timeout))
            .await
            .map_err(|e| e.to_failure(NAME))?;
        let status = res.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(Failure::from_status(
                NAME,
                status,
                format!("Genius song page returned {status}"),
            ));
        }
        let mut buf = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| {
            Failure::common(
                NAME,
                "reading the Genius song page failed",
                Some(e.without_url().to_string()),
            )
        })? {
            if buf.len() + chunk.len() > MAX_PAGE_BYTES {
                return Err(Failure::common(
                    NAME,
                    "Genius song page too large",
                    Some(format!("over {MAX_PAGE_BYTES} bytes")),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    async fn lookup(&self, artist: &str, title: &str) -> Result<Option<Lyrics>, Failure> {
        let hits = self.search(&query(artist, title)).await?;
        let Some(hit) = pick(artist, title, &hits) else {
            return Ok(None);
        };
        if hit.instrumental {
            return Ok(None);
        }
        let Some(page) = self.page(&hit.path).await? else {
            return Ok(None);
        };
        // Parsing a large page is CPU work: keep it off the async workers.
        let text = tokio::task::spawn_blocking(move || extract_lyrics(&page))
            .await
            .map_err(|e| Failure::fault(NAME, "lyrics extraction panicked", Some(e.to_string())))?;
        match text {
            None => Err(Failure::common(
                NAME,
                "no lyrics block on the Genius song page",
                Some(format!("{} (layout changed or blocked page)", hit.path)),
            )),
            Some(t) if t.is_empty() => Ok(None),
            Some(t) => {
                tracing::debug!(plugin = NAME, path = %hit.path, "lyrics found on Genius");
                Ok(Some(Lyrics {
                    text: t.into(),
                    provider: PROVIDER,
                }))
            }
        }
    }
}

#[async_trait]
impl LyricsProvider for GeniusClient {
    fn name(&self) -> &str {
        NAME
    }

    async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        let (artist, title) = (track.author.trim(), track.title.trim());
        if artist.is_empty() || title.is_empty() {
            return Ok(None);
        }
        let key = format!(
            "{}\u{1f}{}",
            words(&clean_artist(artist)).trim(),
            words(&clean_title(title)).trim()
        );
        if let Some(hit) = self.found.get(&key) {
            return Ok(Some(hit));
        }
        if self.missing.contains(&key) {
            return Ok(None);
        }
        match tokio::time::timeout(self.settings.timeout, self.lookup(artist, title)).await {
            Err(_) => Err(Failure::common(NAME, "Genius timed out", None)),
            Ok(Ok(Some(found))) => {
                self.found.set(key, found.clone());
                Ok(Some(found))
            }
            Ok(Ok(None)) => {
                self.missing.set(key, ());
                Ok(None)
            }
            Ok(Err(e)) => Err(e),
        }
    }
}

/// The plugin: registers the Genius lyrics provider unless disabled.
#[derive(Default, Clone)]
pub struct GeniusPlugin {
    /// Fixed settings instead of `WAVYZON_PLUGIN_GENIUS_*` (tests, embedders).
    settings: Option<Settings>,
}

impl GeniusPlugin {
    /// A plugin using `settings` instead of the environment.
    pub fn with_settings(settings: Settings) -> Self {
        Self {
            settings: Some(settings),
        }
    }
}

#[async_trait]
impl Plugin for GeniusPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            description: "Genius lyrics, asked before lyrics.ovh",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let settings = match &self.settings {
            Some(s) => s.clone(),
            None => Settings::from_context(ctx)?,
        };
        if !settings.enabled {
            tracing::info!("genius plugin disabled (WAVYZON_PLUGIN_GENIUS_ENABLED=false), lyrics from lyrics.ovh only");
            return Ok(());
        }
        tracing::info!(
            plugin = NAME,
            auth = settings.auth_mode(),
            "Genius lyrics enabled, before lyrics.ovh"
        );
        reg.lyrics(Arc::new(GeniusClient::new(settings)?));
        Ok(())
    }
}

/// The plugin, for the node's list of compiled-in plugins.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(GeniusPlugin::default())
}

#[cfg(test)]
mod tests;
