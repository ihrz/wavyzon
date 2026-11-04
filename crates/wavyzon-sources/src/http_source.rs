//! The `http` source: direct `http(s)://` links to audio files and
//! Icecast/Shoutcast streams (also `.m3u`/`.pls` radio playlists, one level).
//!
//! Probing does one `GET` with `Range: bytes=0-16383`:
//!
//! - `Content-Type` (or the URL's extension) gives the decoder hint, and
//!   non-audio types (HTML pages…) are refused;
//! - `icy-*` headers, or a `200` without `Content-Length`, mark a live
//!   stream (`is_stream`);
//! - the title comes from `icy-name`, else the file name;
//! - MP3/WAV durations are estimated from the sniffed bytes.
//!
//! Seeking: for byte-seekable formats (MP3, ADTS AAC) with a known duration
//! on a server that honours ranges, [`Playable::open`] requests the
//! proportional byte offset and reports an approximate `start_sample`;
//! otherwise it starts at 0 and the player skips forward.
//!
//! The client refuses private/loopback addresses by default (SSRF guard, on
//! DNS answers and every redirect), caps redirects and bounds every probe.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderMap};
use wavyzon_plugin::{Failure, Format, LoadResult, Playable, Source, Stream, Track};

use crate::httpx::{HttpClient, HttpError, HttpOptions, Req};
use crate::probe::{SNIFF_BYTES, hint_for_mime, hint_for_path, playlist_entry, sniff};
use crate::util::response_body;

pub const NAME: &str = "http";

/// Settings of the HTTP source.
#[derive(Debug, Clone)]
pub struct HttpSourceConfig {
    /// `HTTP_SOURCE_ENABLED` (default `true`).
    pub enabled: bool,
    /// `HTTP_SOURCE_BLOCK_PRIVATE` (default `true`): refuse loopback, private,
    /// link-local… addresses.
    pub block_private: bool,
    /// `HTTP_SOURCE_MAX_REDIRECTS` (default 5).
    pub max_redirects: usize,
    /// `HTTP_SOURCE_TIMEOUT_MS` (default 10 000): connect + probe timeout.
    pub timeout: Duration,
    /// `HTTP_SOURCE_PROXY`: optional proxy.
    pub proxy: Option<String>,
}

impl Default for HttpSourceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            block_private: true,
            max_redirects: 5,
            timeout: Duration::from_secs(10),
            proxy: None,
        }
    }
}

/// The HTTP source.
pub struct HttpSource {
    http: Arc<HttpClient>,
    timeout: Duration,
}

struct Probe {
    headers: HeaderMap,
    status: u16,
    total: Option<u64>,
    head: Vec<u8>,
    final_url: String,
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn is_icy(h: &HeaderMap) -> bool {
    h.keys().any(|k| k.as_str().starts_with("icy-"))
}

fn total_of(status: u16, h: &HeaderMap) -> Option<u64> {
    if status == 206 {
        return header(h, CONTENT_RANGE.as_str())
            .and_then(|r| r.rsplit('/').next())
            .and_then(|t| t.parse().ok());
    }
    header(h, CONTENT_LENGTH.as_str()).and_then(|l| l.parse().ok())
}

fn mime(h: &HeaderMap) -> String {
    header(h, CONTENT_TYPE.as_str())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_playlist_type(m: &str, path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    matches!(
        m,
        "audio/x-mpegurl"
            | "audio/mpegurl"
            | "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/x-scpls"
            | "application/pls+xml"
    ) || p.ends_with(".m3u")
        || p.ends_with(".pls")
}

fn hint(h: &HeaderMap, url: &str) -> Option<&'static str> {
    let path = url::Url::parse(url).map(|u| u.path().to_string()).unwrap_or_default();
    hint_for_mime(&mime(h)).or_else(|| hint_for_path(&path))
}

fn file_title(url: &str) -> String {
    let Ok(u) = url::Url::parse(url) else {
        return url.to_string();
    };
    let name = u
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .unwrap_or_default();
    let name = percent_decode(&name);
    let stem = name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(name);
    if stem.trim().is_empty() {
        u.host_str().unwrap_or(url).to_string()
    } else {
        stem
    }
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

impl HttpSource {
    pub fn new(cfg: &HttpSourceConfig) -> Result<Self, HttpError> {
        let opts = HttpOptions {
            proxy: cfg.proxy.clone(),
            connect_timeout: cfg.timeout.min(Duration::from_secs(10)),
            request_timeout: cfg.timeout,
            header_timeout: cfg.timeout,
            max_redirects: cfg.max_redirects,
            block_private: cfg.block_private,
            ..HttpOptions::default()
        };
        Ok(Self {
            http: Arc::new(HttpClient::new(NAME, opts)?),
            timeout: cfg.timeout,
        })
    }

    async fn probe(&self, url: &str, sniff_body: bool) -> Result<Probe, Failure> {
        let req = Req::get(url)
            .map_err(|e| e.to_failure(NAME))?
            .header("Range", &format!("bytes=0-{}", SNIFF_BYTES - 1));
        let fut = async {
            let mut res = self.http.stream(&req).await.map_err(|e| e.to_failure(NAME))?;
            let status = res.status().as_u16();
            let headers = res.headers().clone();
            let final_url = res.url().to_string();
            let total = total_of(status, &headers);
            let mut head = Vec::new();
            let live = is_icy(&headers) || (status == 200 && total.is_none());
            if (sniff_body && !live) || is_playlist_type(&mime(&headers), res.url().path()) {
                while head.len() < SNIFF_BYTES {
                    match res.chunk().await {
                        Ok(Some(c)) => head.extend_from_slice(&c),
                        _ => break,
                    }
                }
                head.truncate(SNIFF_BYTES);
            }
            Ok(Probe {
                headers,
                status,
                total,
                head,
                final_url,
            })
        };
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| Failure::common(NAME, "timed out", Some(crate::httpx::redact(url))))?
    }

    async fn resolve(&self, url: &str, depth: usize) -> Result<Track, Failure> {
        let p = self.probe(url, true).await?;
        let m = mime(&p.headers);
        let path = url::Url::parse(&p.final_url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        if is_playlist_type(&m, &path) {
            let entry = playlist_entry(&String::from_utf8_lossy(&p.head))
                .filter(|_| depth == 0)
                .ok_or_else(|| {
                    Failure::common(
                        NAME,
                        "unsupported playlist (HLS or empty)",
                        Some(crate::httpx::redact(url)),
                    )
                })?;
            return Box::pin(self.resolve(&entry, depth + 1)).await;
        }
        let hint = hint(&p.headers, &p.final_url);
        let audio_type = m.starts_with("audio/")
            || m == "application/ogg"
            || hint.is_some()
            || ((m.is_empty() || m == "application/octet-stream") && is_icy(&p.headers));
        if !audio_type {
            return Err(Failure::common(
                NAME,
                "not an audio file",
                Some(format!("content type {m:?}")),
            ));
        }
        let live = is_icy(&p.headers) || (p.status == 200 && p.total.is_none());
        let duration_ms = if live {
            0
        } else {
            sniff(&p.head, p.total, hint).duration_ms
        };
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        Ok(Track {
            source: NAME.into(),
            identifier: url.to_string(),
            title: header(&p.headers, "icy-name")
                .map(str::to_string)
                .unwrap_or_else(|| file_title(url)),
            author: header(&p.headers, "icy-description")
                .or(header(&p.headers, "icy-genre"))
                .map(str::to_string)
                .unwrap_or(host),
            duration_ms,
            uri: url.to_string(),
            is_stream: live,
            ..Default::default()
        })
    }

    /// Byte offset for `start_ms`, when the server supports ranges and the
    /// format decodes from any offset.
    async fn seek_offset(&self, track: &Track, start_ms: i64) -> Option<u64> {
        if start_ms <= 0 || track.is_stream || track.duration_ms <= 0 || start_ms >= track.duration_ms {
            return None;
        }
        let p = self.probe(&track.identifier, true).await.ok()?;
        let ranges =
            p.status == 206 || header(&p.headers, "accept-ranges").is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
        let total = p.total?;
        let s = sniff(&p.head, Some(total), hint(&p.headers, &p.final_url));
        if !ranges || !s.byte_seekable || total <= s.audio_start {
            return None;
        }
        Some(s.audio_start + (total - s.audio_start) * start_ms as u64 / track.duration_ms as u64)
    }
}

#[async_trait]
impl Source for HttpSource {
    fn name(&self) -> &str {
        NAME
    }

    /// Any `http(s)://` URL; the resolver only asks this source after every
    /// other one declined.
    fn matches_url(&self, url: &str) -> bool {
        url.starts_with("http://") || url.starts_with("https://")
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        Ok(LoadResult::single(self.resolve(url, 0).await?))
    }

    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        self.resolve(identifier, 0).await
    }

    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for HttpSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        if !self.matches_url(&track.identifier) {
            return Err(Failure::common(NAME, "not an http url", None));
        }
        let offset = self.seek_offset(track, start_ms).await;
        let mut req = Req::get(&track.identifier).map_err(|e| e.to_failure(NAME))?;
        if let Some(o) = offset {
            req = req.header("Range", &format!("bytes={o}-"));
        }
        let res = self.http.stream(&req).await.map_err(|e| e.to_failure(NAME))?;
        let status = res.status().as_u16();
        let h = res.headers().clone();
        let start_sample = if offset.is_some() && status == 206 {
            start_ms * 48
        } else {
            0
        };
        let live = track.is_stream || is_icy(&h) || (status == 200 && total_of(status, &h).is_none());
        let hint = hint(&h, res.url().as_str()).map(str::to_string);
        tracing::debug!(source = NAME, url = %crate::httpx::redact(&track.identifier), status, start_sample, live, "opened http stream");
        Ok(Stream {
            body: response_body(res),
            format: Format::Encoded { hint },
            start_sample,
            live,
        })
    }

    fn seekable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::tests::fake_mp3;
    use crate::testutil::serve;
    use axum::Router;
    use axum::http::{HeaderMap as AxHeaders, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tokio::io::AsyncReadExt;

    const MP3_LEN: usize = 20 + 16_000 * 10;

    fn ranged(h: &AxHeaders, body: &'static [u8], ct: &'static str) -> axum::response::Response {
        let range = h
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|r| r.strip_prefix("bytes="));
        match range {
            Some(r) => {
                let (a, b) = r.split_once('-').unwrap();
                let a: usize = a.parse().unwrap();
                let b: usize = if b.is_empty() {
                    body.len() - 1
                } else {
                    b.parse::<usize>().unwrap().min(body.len() - 1)
                };
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, ct.to_string()),
                        (header::CONTENT_RANGE, format!("bytes {a}-{b}/{}", body.len())),
                        (header::ACCEPT_RANGES, "bytes".into()),
                    ],
                    body[a..=b].to_vec(),
                )
                    .into_response()
            }
            None => ([(header::CONTENT_TYPE, ct)], body.to_vec()).into_response(),
        }
    }

    async fn server() -> String {
        let mp3: &'static [u8] = Box::leak(fake_mp3(MP3_LEN).into_boxed_slice());
        let app = Router::new()
            .route(
                "/music/{file}",
                get(move |h: AxHeaders| async move { ranged(&h, mp3, "audio/mpeg") }),
            )
            .route(
                "/noranges.mp3",
                get(move || async move { ([(header::CONTENT_TYPE, "audio/mpeg")], mp3.to_vec()) }),
            )
            .route(
                "/radio",
                get(|| async {
                    let body = axum::body::Body::from_stream(futures::stream::iter(
                        (0..4).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"ICYDATA"))),
                    ));
                    (
                        [
                            (header::CONTENT_TYPE, "audio/aacp"),
                            (header::HeaderName::from_static("icy-name"), "Cool FM"),
                            (header::HeaderName::from_static("icy-br"), "128"),
                        ],
                        body,
                    )
                }),
            )
            .route(
                "/page",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<html></html>") }),
            )
            .route(
                "/radio.pls",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "audio/x-scpls")],
                        "[playlist]\nFile1=http://127.0.0.1:1/unused\n",
                    )
                }),
            );
        serve(app).await
    }

    fn open_source() -> HttpSource {
        HttpSource::new(&HttpSourceConfig {
            block_private: false,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn probes_files_streams_and_refuses_pages() {
        let base = server().await;
        let src = open_source();
        let t = src.track(&format!("{base}/music/My%20Song.mp3")).await.unwrap();
        assert_eq!(t.title, "My Song");
        assert_eq!(t.duration_ms, 10_000);
        assert!(!t.is_stream);
        let r = src.track(&format!("{base}/radio")).await.unwrap();
        assert_eq!(r.title, "Cool FM");
        assert!(r.is_stream);
        let err = src.track(&format!("{base}/page")).await.unwrap_err();
        assert_eq!(err.message, "not an audio file");
        assert!(
            src.track(&format!("{base}/radio.pls")).await.is_err(),
            "playlist entry to a dead port fails"
        );
    }

    #[tokio::test]
    async fn open_seeks_by_range_when_possible() {
        let base = server().await;
        let src = open_source();
        let t = src.track(&format!("{base}/music/My%20Song.mp3")).await.unwrap();
        let mut s = src.open(&t, 5_000).await.unwrap();
        assert_eq!(s.start_sample, 5_000 * 48);
        assert_eq!(s.format, Format::mp3());
        let mut body = Vec::new();
        s.body.read_to_end(&mut body).await.unwrap();
        assert_eq!(body.len(), MP3_LEN - (20 + (MP3_LEN - 20) / 2));

        let mut nr = t.clone();
        nr.identifier = format!("{base}/noranges.mp3");
        let s = src.open(&nr, 5_000).await.unwrap();
        assert_eq!(s.start_sample, 0, "no range support: the player skips");

        let r = src.track(&format!("{base}/radio")).await.unwrap();
        let mut s = src.open(&r, 0).await.unwrap();
        assert!(s.live);
        assert_eq!(
            s.format,
            Format::Encoded {
                hint: Some("aac".into())
            }
        );
        let mut body = Vec::new();
        s.body.read_to_end(&mut body).await.unwrap();
        assert_eq!(body.len(), 4 * 7);
    }

    #[tokio::test]
    async fn private_addresses_are_refused_by_default() {
        let base = server().await;
        let src = HttpSource::new(&HttpSourceConfig::default()).unwrap();
        let err = src.track(&format!("{base}/music/My%20Song.mp3")).await.unwrap_err();
        assert_eq!(err.message, "address not allowed");
        let t = Track {
            source: NAME.into(),
            identifier: format!("{base}/radio"),
            ..Default::default()
        };
        assert!(
            src.open(&t, 0).await.is_err(),
            "crafted tracks cannot reach private hosts either"
        );
    }
}
