//! Track lyrics from [LRCLIB](https://lrclib.net) (no key), synced when it has
//! them.
//!
//! The track's own artist, title and duration are asked first
//! (`GET /api/get`, which matches durations within ±2 s); when that finds
//! nothing, `GET /api/search` with the cleaned title and first artist, keeping
//! a record with the same title and artist and a duration within
//! [`DURATION_SLACK_MS`], synced records first. Synced lyrics (LRC) become
//! LavaLyrics `lines`. Hits and misses are cached, failures are not.
//!
//! LRCLIB asks clients to identify themselves: requests carry
//! `Wavyzon v<version> (https://gitlab.com/ihrz/wavyzon)` as User-Agent.
//!
//! Not in Cadence. In the lyrics chain it comes after plugin providers
//! (Genius) and before lyrics.ovh (see [`crate::Sources::lyrics`]).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use wavyzon_plugin::{Failure, Lyrics, LyricsLine, LyricsProvider, Track, async_trait};

use crate::cache::Lru;
use crate::httpx::{HttpClient, HttpError, Req};
use crate::lyrics::{clean_artist, clean_title, tidy};
use crate::metrics::{Metrics, inc};
use crate::util::bad_json;

/// Source name used in failures, logs and the HTTP client.
pub const NAME: &str = "lrclib";
/// What the API reports as the lyrics provider.
pub const PROVIDER: &str = "lrclib.net";
/// Default API base (`LRCLIB_API_URL` overrides it).
pub const DEFAULT_API: &str = "https://lrclib.net/api";
/// Largest gap between the track's and a search result's duration.
pub const DURATION_SLACK_MS: i64 = 3_000;

const ENTRIES: usize = 20_000;
/// Lyrics of a recording do not change.
const FOUND_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// LRCLIB fetches missing songs in the background: ask again later.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
/// Never hold a request longer than this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// The User-Agent LRCLIB asks for.
pub fn user_agent() -> String {
    format!(
        "Wavyzon v{} (https://gitlab.com/ihrz/wavyzon)",
        env!("CARGO_PKG_VERSION")
    )
}

/// One LRCLIB lyrics record.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    /// Seconds.
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    plain_lyrics: Option<String>,
    #[serde(default)]
    synced_lyrics: Option<String>,
}

impl Record {
    fn has_synced(&self) -> bool {
        self.synced_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty())
    }
    fn duration_ms(&self) -> i64 {
        (self.duration * 1000.0).round() as i64
    }
}

/// Lowercase words of letters and digits, single-spaced and padded with
/// spaces, so `contains(" queen ")` matches whole words.
fn words(s: &str) -> String {
    let mut out = String::from(" ");
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    if !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

/// `mm:ss`, `mm:ss.x`, `mm:ss.xx` or `mm:ss.xxx` in ms.
fn parse_stamp(tag: &str) -> Option<i64> {
    let (m, s) = tag.split_once(':')?;
    let m: i64 = m.trim().parse().ok()?;
    let (s, frac) = s.split_once('.').unwrap_or((s, ""));
    let s: i64 = s.parse().ok()?;
    if !(0..60).contains(&s) || m < 0 || !frac.chars().all(|c| c.is_ascii_digit()) || frac.len() > 3 {
        return None;
    }
    let frac_ms = match frac.len() {
        0 => 0,
        n => frac.parse::<i64>().ok()? * 10i64.pow(3 - n as u32),
    };
    Some(m * 60_000 + s * 1000 + frac_ms)
}

/// Timed lines of an LRC text, in time order. A line may carry several
/// timestamps (`[00:10.00][01:20.00]chorus`); metadata tags (`[ar:…]`) and
/// untimed lines are skipped. Each line lasts until the next one; the last
/// until `track_ms` when known, else 0.
pub fn parse_lrc(lrc: &str, track_ms: i64) -> Vec<LyricsLine> {
    let mut timed: Vec<(i64, &str)> = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            match parse_stamp(&rest[1..end]) {
                Some(ms) => stamps.push(ms),
                None => {
                    stamps.clear();
                    break;
                }
            }
            rest = &rest[end + 1..];
        }
        let text = rest.trim();
        timed.extend(stamps.into_iter().map(|ms| (ms, text)));
    }
    timed.sort_by_key(|(ms, _)| *ms);
    let mut out: Vec<LyricsLine> = Vec::with_capacity(timed.len());
    for (i, (ms, text)) in timed.iter().enumerate() {
        let end = timed.get(i + 1).map(|(next, _)| *next).unwrap_or(track_ms);
        out.push(LyricsLine {
            timestamp_ms: *ms,
            duration_ms: (end - ms).max(0),
            line: (*text).into(),
        });
    }
    out
}

/// The record as lyrics: `None` for an instrumental or an empty record.
fn to_lyrics(r: &Record, track_ms: i64) -> Option<Lyrics> {
    if r.instrumental {
        return None;
    }
    let length = if track_ms > 0 { track_ms } else { r.duration_ms() };
    let lines = r
        .synced_lyrics
        .as_deref()
        .map(|s| parse_lrc(s, length))
        .unwrap_or_default();
    let plain = tidy(r.plain_lyrics.as_deref().unwrap_or_default());
    let text = if plain.is_empty() {
        tidy(&lines.iter().map(|l| &*l.line).collect::<Vec<_>>().join("\n"))
    } else {
        plain
    };
    if text.is_empty() {
        return None;
    }
    Some(Lyrics {
        text: text.into(),
        provider: PROVIDER,
        lines: lines.into(),
    })
}

/// The best search record for the track: same title and artist, duration
/// within [`DURATION_SLACK_MS`] when both are known; synced first, then the
/// closest duration.
fn best<'a>(artist: &str, title: &str, track_ms: i64, records: &'a [Record]) -> Option<&'a Record> {
    let (want_t, want_a) = (words(&clean_title(title)), words(&clean_artist(artist)));
    if want_t.trim().is_empty() || want_a.trim().is_empty() {
        return None;
    }
    let gap = |r: &Record| (r.duration_ms() - track_ms).abs();
    records
        .iter()
        .filter(|r| !r.instrumental)
        .filter(|r| words(&clean_title(&r.track_name)) == want_t)
        .filter(|r| {
            let got = words(&r.artist_name);
            got.contains(&want_a) || want_a.contains(&got)
        })
        .filter(|r| track_ms <= 0 || r.duration <= 0.0 || gap(r) <= DURATION_SLACK_MS)
        .min_by_key(|r| (!r.has_synced(), if track_ms > 0 { gap(r) } else { 0 }))
}

/// The LRCLIB client with its caches.
pub struct LrclibClient {
    http: Arc<HttpClient>,
    base: String,
    found: Lru<String, Lyrics>,
    missing: Lru<String, ()>,
    metrics: Arc<Metrics>,
}

impl LrclibClient {
    /// A client for `base` (`https://lrclib.net/api`, or a test server). The
    /// client should send [`user_agent`].
    pub fn new(http: Arc<HttpClient>, base: impl Into<String>, metrics: Arc<Metrics>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            found: Lru::new(ENTRIES, FOUND_TTL),
            missing: Lru::new(ENTRIES, MISSING_TTL),
            metrics,
        }
    }

    /// Lyrics of `track`, `None` when LRCLIB has none. A network or API
    /// failure is an error and is not cached.
    pub async fn get(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        let (artist, title) = (track.author.trim(), track.title.trim());
        if artist.is_empty() || title.is_empty() {
            inc(&self.metrics.lrclib.not_found);
            return Ok(None);
        }
        let track_ms = track.duration_ms.max(0);
        // Versions of a song differ in length, and synced lines with them.
        let key = format!(
            "{}\u{1f}{}\u{1f}{}",
            words(&clean_artist(artist)).trim(),
            words(&clean_title(title)).trim(),
            track_ms / 1000
        );
        if let Some(hit) = self.found.get(&key) {
            inc(&self.metrics.lrclib.cached);
            return Ok(Some(hit));
        }
        if self.missing.contains(&key) {
            inc(&self.metrics.lrclib.not_found);
            return Ok(None);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, self.lookup(artist, title, track_ms)).await {
            Err(_) => {
                inc(&self.metrics.lrclib.failed);
                Err(Failure::common(NAME, "LRCLIB timed out", None))
            }
            Ok(Err(e)) => {
                inc(&self.metrics.lrclib.failed);
                tracing::warn!(error = %e, "LRCLIB lookup failed");
                Err(e)
            }
            Ok(Ok(None)) => {
                inc(&self.metrics.lrclib.not_found);
                self.missing.set(key, ());
                Ok(None)
            }
            Ok(Ok(Some(l))) => {
                inc(&self.metrics.lrclib.found);
                if !l.lines.is_empty() {
                    inc(&self.metrics.lrclib.synced);
                }
                self.found.set(key, l.clone());
                Ok(Some(l))
            }
        }
    }

    async fn lookup(&self, artist: &str, title: &str, track_ms: i64) -> Result<Option<Lyrics>, Failure> {
        let secs = (track_ms + 500) / 1000;
        let mut pairs = vec![("track_name", title.to_string()), ("artist_name", artist.to_string())];
        // LRCLIB takes 1..=3600 s; a stream or an unknown length is left out.
        if (1..=3600).contains(&secs) {
            pairs.push(("duration", secs.to_string()));
        }
        if let Some(r) = self.request::<Record>("get", &pairs).await? {
            if let Some(l) = to_lyrics(&r, track_ms) {
                return Ok(Some(l));
            }
            if r.instrumental {
                return Ok(None);
            }
        }
        let pairs = [
            ("track_name", clean_title(title)),
            ("artist_name", clean_artist(artist)),
        ];
        let records = self.request::<Vec<Record>>("search", &pairs).await?.unwrap_or_default();
        Ok(best(artist, title, track_ms, &records).and_then(|r| to_lyrics(r, track_ms)))
    }

    /// `GET {base}/{path}?{pairs}`; `None` on a 404 (LRCLIB's "not found").
    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        pairs: &[(&str, String)],
    ) -> Result<Option<T>, Failure> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs.iter().map(|(k, v)| (*k, v.as_str())))
            .finish();
        let url = format!("{}/{path}?{query}", self.base);
        let req = Req::get(&url)
            .map_err(|e| e.to_failure(NAME))?
            .header("Accept", "application/json");
        let body = match self.http.fetch(&req).await {
            Ok(body) => body,
            Err(HttpError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e.to_failure(NAME)),
        };
        serde_json::from_slice(&body).map(Some).map_err(|e| bad_json(NAME, e))
    }
}

#[async_trait]
impl LyricsProvider for LrclibClient {
    fn name(&self) -> &str {
        NAME
    }

    async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        self.get(track).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;

    use super::*;
    use crate::httpx::HttpOptions;
    use crate::testutil::serve;

    fn track(author: &str, title: &str, duration_ms: i64) -> Track {
        Track {
            source: "deezer".into(),
            identifier: "1".into(),
            author: author.into(),
            title: title.into(),
            duration_ms,
            ..Default::default()
        }
    }

    const LRC: &str =
        "[ar:Queen]\n[00:01.50]Mm ba ba de\n[00:04.20][00:20.00]Pressure\n\n[00:10.05]Pushing down on me\n[00:30.00]";

    fn record(title: &str, artist: &str, secs: f64, synced: bool) -> serde_json::Value {
        json!({
            "id": 1, "trackName": title, "artistName": artist, "albumName": "Hot Space", "duration": secs,
            "instrumental": false, "plainLyrics": "Mm ba ba de\nPressure\nPushing down on me",
            "syncedLyrics": if synced { json!(LRC) } else { json!(null) },
        })
    }

    /// A fake LRCLIB: `/get` knows "Queen / Under Pressure" at 248 s, `/search`
    /// knows a remastered upload, "Broken" fails; requests are counted and the
    /// User-Agent is recorded.
    async fn fake() -> (LrclibClient, Arc<AtomicU32>, Arc<std::sync::Mutex<String>>) {
        let calls = Arc::new(AtomicU32::new(0));
        let ua = Arc::new(std::sync::Mutex::new(String::new()));
        let (c1, c2, u1) = (calls.clone(), calls.clone(), ua.clone());
        let router = Router::new()
            .route(
                "/api/get",
                get(move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| {
                    let (c, u) = (c1.clone(), u1.clone());
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        *u.lock().unwrap() = h["user-agent"].to_str().unwrap().to_string();
                        let dur_ok = q.get("duration").is_none_or(|d| d.parse::<i64>().unwrap().abs_diff(248) <= 2);
                        match (q["artist_name"].as_str(), q["track_name"].as_str()) {
                            ("Queen", "Under Pressure") if dur_ok => {
                                axum::Json(record("Under Pressure", "Queen", 248.0, true)).into_response()
                            }
                            ("Broken", _) => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
                            _ => (
                                StatusCode::NOT_FOUND,
                                r#"{"message":"Failed to find specified track","name":"TrackNotFound","statusCode":404}"#,
                            )
                                .into_response(),
                        }
                    }
                }),
            )
            .route(
                "/api/search",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let c = c2.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        if q["track_name"] == "Bohemian Rhapsody" && q["artist_name"] == "Queen" {
                            axum::Json(json!([
                                record("Bohemian Rhapsody (Live)", "Queen", 360.0, true),
                                record("Bohemian Rhapsody", "Queen", 600.0, true),
                                record("Bohemian Rhapsody", "Queen", 355.0, false),
                                record("Bohemian Rhapsody", "Queen", 354.0, true),
                            ]))
                        } else {
                            axum::Json(json!([]))
                        }
                    }
                }),
            );
        let base = serve(router).await;
        let opts = HttpOptions {
            user_agent: user_agent(),
            ..HttpOptions::default()
        };
        let http = Arc::new(HttpClient::new(NAME, opts).unwrap());
        (
            LrclibClient::new(http, format!("{base}/api/"), Arc::new(Metrics::default())),
            calls,
            ua,
        )
    }

    #[tokio::test]
    async fn exact_match_gives_synced_lines_and_is_cached() {
        let (lc, calls, ua) = fake().await;
        let got = lc
            .get(&track("Queen", "Under Pressure", 248_400))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.provider, PROVIDER);
        assert_eq!(&*got.text, "Mm ba ba de\nPressure\nPushing down on me");
        let lines: Vec<(i64, i64, &str)> = got
            .lines
            .iter()
            .map(|l| (l.timestamp_ms, l.duration_ms, &*l.line))
            .collect();
        assert_eq!(
            lines,
            [
                (1_500, 2_700, "Mm ba ba de"),
                (4_200, 5_850, "Pressure"),
                (10_050, 9_950, "Pushing down on me"),
                (20_000, 10_000, "Pressure"),
                (30_000, 218_400, ""),
            ]
        );
        assert!(ua.lock().unwrap().starts_with("Wavyzon v"), "{}", ua.lock().unwrap());
        assert_eq!(
            lc.get(&track("Queen", "Under Pressure", 248_400)).await.unwrap(),
            Some(got)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(lc.metrics.lrclib.synced.load(Ordering::Relaxed), 1);
        assert_eq!(lc.metrics.lrclib.cached.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn search_picks_same_song_close_duration_synced_first() {
        let (lc, calls, _) = fake().await;
        let got = lc
            .get(&track("Queen", "Bohemian Rhapsody - Remastered 2011", 355_000))
            .await
            .unwrap()
            .unwrap();
        // The 354 s synced record beats the 355 s plain one; live and 600 s are out.
        assert!(!got.lines.is_empty());
        assert_eq!(got.lines.last().unwrap().duration_ms, 325_000);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "get, then search");
    }

    #[tokio::test]
    async fn misses_are_cached_and_failures_are_not() {
        let (lc, calls, _) = fake().await;
        assert_eq!(lc.get(&track("Nobody", "Nothing", 100_000)).await.unwrap(), None);
        assert_eq!(lc.get(&track("Nobody", "Nothing", 100_000)).await.unwrap(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the miss was asked twice");
        // Another length is another recording.
        assert_eq!(lc.get(&track("Nobody", "Nothing", 200_000)).await.unwrap(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(lc.get(&track("Broken", "Song", 0)).await.is_err());
        assert!(lc.get(&track("Broken", "Song", 0)).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        assert_eq!(lc.metrics.lrclib.failed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn lrc_stamps_and_tags() {
        assert_eq!(parse_stamp("01:02.5"), Some(62_500));
        assert_eq!(parse_stamp("01:02.05"), Some(62_050));
        assert_eq!(parse_stamp("01:02.005"), Some(62_005));
        assert_eq!(parse_stamp("01:02"), Some(62_000));
        assert_eq!(parse_stamp("ar:Queen"), None);
        assert_eq!(parse_stamp("01:72.00"), None);
        let lines = parse_lrc("[ti:x]\nno stamp\n[00:02.00]b\n[00:01.00]a", 0);
        let got: Vec<_> = lines
            .iter()
            .map(|l| (l.timestamp_ms, l.duration_ms, &*l.line))
            .collect();
        assert_eq!(got, [(1_000, 1_000, "a"), (2_000, 0, "b")]);
    }

    #[test]
    fn instrumental_and_empty_records_are_none() {
        let mut r = Record {
            track_name: "x".into(),
            artist_name: "y".into(),
            instrumental: true,
            ..Default::default()
        };
        assert_eq!(to_lyrics(&r, 0), None);
        r.instrumental = false;
        assert_eq!(to_lyrics(&r, 0), None);
        r.synced_lyrics = Some("[00:01.00]only synced".into());
        let l = to_lyrics(&r, 0).unwrap();
        assert_eq!(&*l.text, "only synced");
        assert_eq!(l.lines.len(), 1);
    }
}
