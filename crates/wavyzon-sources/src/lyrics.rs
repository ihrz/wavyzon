//! Track lyrics from [lyrics.ovh](https://lyrics.ovh) (no key, plain text).
//!
//! lyrics.ovh looks a song up by artist and title. The track's own pair is
//! tried first; when that finds nothing, a cleaned pair is tried once (first
//! credited artist, title without "(feat. ...)", "- Remastered 2011",
//! "(Official Video)"...). Both answers are cached, a miss for less time than
//! a hit, so a busy node does not ask twice for the same song.
//!
//! Not in Cadence: Wavyzon serves them through the LavaLyrics API shape so
//! Lavalink clients can read them (see the node's `/v4/lyrics`).
//!
//! lyrics.ovh is the last provider of [`crate::Sources::lyrics`]: plugin
//! providers (Genius) are asked first, and it answers when they have nothing.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
pub use wavyzon_plugin::Lyrics;
use wavyzon_plugin::{Failure, LyricsProvider, Track, async_trait};

use crate::cache::Lru;
use crate::httpx::{HttpClient, HttpError};
use crate::metrics::{Metrics, inc};
use crate::util::{bad_json, esc};

/// Source name used in failures, logs and the HTTP client.
pub const NAME: &str = "lyrics";
/// What the API reports as the lyrics provider.
pub const PROVIDER: &str = "lyrics.ovh";
/// Default API base (`LYRICS_API_URL` overrides it).
pub const DEFAULT_API: &str = "https://api.lyrics.ovh/v1";

const ENTRIES: usize = 20_000;
/// Lyrics of a song do not change.
const FOUND_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// A song may be added to lyrics.ovh later.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
/// lyrics.ovh is slow at times: never hold a request longer than this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// The lyrics.ovh client with its caches.
pub struct LyricsClient {
    http: Arc<HttpClient>,
    base: String,
    found: Lru<String, Lyrics>,
    missing: Lru<String, ()>,
    metrics: Arc<Metrics>,
}

fn noise_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)remaster|official|video|audio|lyric|\blive\b|version|edit\b|\bmix\b|feat\.?|\bft\.?|\bwith\b")
            .unwrap()
    })
}

fn brackets_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s*[(\[][^)\]]*[)\]]").unwrap())
}

fn artist_split_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\s*(?:,|&|\bfeat\.?\s|\bft\.?\s)\s*").unwrap())
}

/// The title without bracketed or dashed extras that only describe the
/// recording ("(feat. X)", "- Remastered 2011"); other brackets stay.
pub fn clean_title(title: &str) -> String {
    let t = brackets_re().replace_all(title, |c: &regex::Captures| {
        if noise_re().is_match(&c[0]) {
            String::new()
        } else {
            c[0].to_string()
        }
    });
    let t = match t.rsplit_once(" - ") {
        Some((head, tail)) if noise_re().is_match(tail) => head.to_string(),
        _ => t.into_owned(),
    };
    t.trim().to_string()
}

/// The first credited artist, without YouTube channel suffixes.
pub fn clean_artist(artist: &str) -> String {
    let a = artist.trim();
    let a = a.strip_suffix(" - Topic").unwrap_or(a);
    let a = a.strip_suffix("VEVO").unwrap_or(a);
    artist_split_re().split(a).next().unwrap_or(a).trim().to_string()
}

/// Normalized text: `\n` line ends, at most one blank line in a row, and no
/// "Paroles de la chanson X par Y" header (some lyrics.ovh entries have one).
fn tidy(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for (i, line) in text.lines().enumerate() {
        if i == 0 && line.starts_with("Paroles de la chanson ") && line.contains(" par ") {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

impl LyricsClient {
    /// A client for `base` (`https://api.lyrics.ovh/v1`, or a test server).
    pub fn new(http: Arc<HttpClient>, base: impl Into<String>, metrics: Arc<Metrics>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            found: Lru::new(ENTRIES, FOUND_TTL),
            missing: Lru::new(ENTRIES, MISSING_TTL),
            metrics,
        }
    }

    /// Lyrics of `track` (its `author` and `title`), `None` when lyrics.ovh
    /// has none. A network or API failure is an error and is not cached.
    pub async fn get(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        let (artist, title) = (track.author.trim(), track.title.trim());
        if artist.is_empty() || title.is_empty() {
            inc(&self.metrics.lyrics.not_found);
            return Ok(None);
        }
        let mut pairs = vec![(artist.to_string(), title.to_string())];
        let cleaned = (clean_artist(artist), clean_title(title));
        if !cleaned.0.is_empty() && !cleaned.1.is_empty() && cleaned != pairs[0] {
            pairs.push(cleaned);
        }
        let key = |(a, t): &(String, String)| format!("{}\u{1f}{}", a.to_lowercase(), t.to_lowercase());
        for pair in &pairs {
            if let Some(hit) = self.found.get(&key(pair)) {
                inc(&self.metrics.lyrics.cached);
                return Ok(Some(hit));
            }
        }
        // lyrics.ovh takes about 5 s to answer a miss (it runs its own
        // fallback search), so both pairs are asked at once rather than one
        // after the other.
        let ask: Vec<&(String, String)> = pairs.iter().filter(|p| self.missing.get(&key(p)).is_none()).collect();
        let answers = futures::future::join_all(ask.iter().map(|(a, t)| self.fetch(a, t))).await;
        let mut failure = None;
        // Answers come back in pair order: the track's own pair wins.
        for (pair, answer) in ask.into_iter().zip(answers) {
            match answer {
                Ok(Some(found)) => {
                    inc(&self.metrics.lyrics.found);
                    self.found.set(key(pair), found.clone());
                    return Ok(Some(found));
                }
                Ok(None) => self.missing.set(key(pair), ()),
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            inc(&self.metrics.lyrics.failed);
            tracing::warn!(error = %e, "lyrics lookup failed");
            return Err(e);
        }
        inc(&self.metrics.lyrics.not_found);
        Ok(None)
    }

    async fn fetch(&self, artist: &str, title: &str) -> Result<Option<Lyrics>, Failure> {
        let url = format!("{}/{}/{}", self.base, esc(artist), esc(title));
        let req = crate::httpx::Req::get(&url).map_err(|e| e.to_failure(NAME))?;
        let body = match tokio::time::timeout(REQUEST_TIMEOUT, self.http.fetch(&req)).await {
            Err(_) => return Err(Failure::common(NAME, "lyrics.ovh timed out", None)),
            // lyrics.ovh answers 404 `{"error":"No lyrics found"}` for a miss.
            Ok(Err(HttpError::Status { status: 404, .. })) => return Ok(None),
            Ok(Err(e)) => return Err(e.to_failure(NAME)),
            Ok(Ok(body)) => body,
        };
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            lyrics: Option<String>,
        }
        let answer: Answer = serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e))?;
        let text = tidy(answer.lyrics.as_deref().unwrap_or_default());
        Ok((!text.is_empty()).then(|| Lyrics {
            text: text.into(),
            provider: PROVIDER,
        }))
    }
}

#[async_trait]
impl LyricsProvider for LyricsClient {
    fn name(&self) -> &str {
        PROVIDER
    }

    async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        self.get(track).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;
    use crate::httpx::HttpOptions;
    use crate::testutil::serve;

    fn track(author: &str, title: &str) -> Track {
        Track {
            source: "deezer".into(),
            identifier: "1".into(),
            author: author.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    /// A fake lyrics.ovh knowing "Queen / Under Pressure" and failing on
    /// "Broken / *"; counts its requests.
    async fn fake() -> (LyricsClient, Arc<AtomicU32>, Arc<Metrics>) {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let router = Router::new().route(
            "/v1/{artist}/{title}",
            get(move |Path((artist, title)): Path<(String, String)>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    match (artist.as_str(), title.as_str()) {
                        ("Queen", "Under Pressure") => (
                            StatusCode::OK,
                            r#"{"lyrics":"Paroles de la chanson Under Pressure par Queen\r\nPressure\r\n\r\n\r\nPushing down on me"}"#,
                        )
                            .into_response(),
                        ("Broken", _) => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
                        _ => (StatusCode::NOT_FOUND, r#"{"error":"No lyrics found"}"#).into_response(),
                    }
                }
            }),
        );
        let base = serve(router).await;
        let http = Arc::new(HttpClient::new(NAME, HttpOptions::default()).unwrap());
        let metrics = Arc::new(Metrics::default());
        (
            LyricsClient::new(http, format!("{base}/v1/"), metrics.clone()),
            calls,
            metrics,
        )
    }

    #[tokio::test]
    async fn finds_tidies_and_caches_lyrics() {
        let (lc, calls, metrics) = fake().await;
        let got = lc.get(&track("Queen", "Under Pressure")).await.unwrap().unwrap();
        assert_eq!(&*got.text, "Pressure\n\nPushing down on me");
        assert_eq!(got.provider, PROVIDER);
        assert_eq!(lc.get(&track("Queen", "Under Pressure")).await.unwrap(), Some(got));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second lookup was not cached");
        assert_eq!(metrics.lyrics.found.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.lyrics.cached.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn falls_back_to_the_cleaned_artist_and_title() {
        let (lc, calls, _) = fake().await;
        let got = lc
            .get(&track("Queen, David Bowie", "Under Pressure - Remastered 2011"))
            .await
            .unwrap();
        assert!(got.is_some(), "the cleaned pair was not tried");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_miss_is_cached_and_a_failure_is_not() {
        let (lc, calls, metrics) = fake().await;
        assert_eq!(lc.get(&track("Nobody", "Nothing")).await.unwrap(), None);
        assert_eq!(lc.get(&track("Nobody", "Nothing")).await.unwrap(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the miss was asked twice");
        assert_eq!(metrics.lyrics.not_found.load(Ordering::Relaxed), 2);

        assert!(lc.get(&track("Broken", "Song")).await.is_err());
        assert!(lc.get(&track("Broken", "Song")).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "a failure was cached");
        assert_eq!(metrics.lyrics.failed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cleaning_keeps_the_song_and_drops_the_recording_details() {
        assert_eq!(clean_title("Under Pressure - Remastered 2011"), "Under Pressure");
        assert_eq!(clean_title("One More Time (Official Video)"), "One More Time");
        assert_eq!(clean_title("Stay (feat. Justin Bieber)"), "Stay");
        assert_eq!(clean_title("Hello (Adele cover)"), "Hello (Adele cover)");
        assert_eq!(clean_title("Ain't No Mountain - Mono"), "Ain't No Mountain - Mono");
        assert_eq!(clean_artist("Queen, David Bowie"), "Queen");
        assert_eq!(clean_artist("Daft Punk - Topic"), "Daft Punk");
        assert_eq!(clean_artist("Simon & Garfunkel"), "Simon");
        assert_eq!(clean_artist("Lil Nas X"), "Lil Nas X");
    }
}
