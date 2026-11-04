//! Per-source HTTP clients.
//!
//! Port of Cadence's `internal/httpx`:
//!
//! - each source gets its own [`HttpClient`] with its own connection pool;
//! - an optional proxy: when a request through it fails at the transport
//!   level, the proxy is skipped for [`PROXY_COOLDOWN`] and the request is
//!   retried directly;
//! - a request that fails on a dead pooled connection (reset, closed before
//!   the response) is retried once;
//! - errors never carry query strings or credentials ([`safe_url`]), since
//!   SoundCloud client ids and signed CDN URLs live there.
//!
//! New in Wavyzon: an SSRF guard ([`HttpOptions::block_private`]) that refuses
//! to connect to loopback/private/link-local addresses, checked both on DNS
//! results and on every redirect hop, and a redirect cap.

use std::error::Error as StdError;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::{Method, Response};
use url::{Host, Url};
use wavyzon_plugin::Failure;

/// How long a failed proxy is bypassed.
pub const PROXY_COOLDOWN: Duration = Duration::from_secs(60);
/// Largest body [`HttpClient::fetch`] reads into memory.
pub const MAX_BODY: usize = 32 << 20;
const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// Tunables of an [`HttpClient`].
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Proxy URL (`http://`, `https://`, `socks5://`). Empty/`None` = direct.
    pub proxy: Option<String>,
    pub connect_timeout: Duration,
    /// Whole-request timeout of [`HttpClient::fetch`]/[`HttpClient::get`].
    pub request_timeout: Duration,
    /// Time to wait for response headers in [`HttpClient::stream`].
    pub header_timeout: Duration,
    /// Max silence between two body reads of a stream.
    pub read_timeout: Duration,
    pub max_redirects: usize,
    /// Refuse loopback, private, link-local, CGNAT, multicast and unspecified
    /// addresses (SSRF guard). Applies to direct connections only.
    pub block_private: bool,
    pub user_agent: String,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            proxy: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(15),
            header_timeout: Duration::from_secs(6),
            read_timeout: Duration::from_secs(30),
            max_redirects: 10,
            block_private: false,
            user_agent: BROWSER_UA.to_string(),
        }
    }
}

impl HttpOptions {
    pub fn with_proxy(proxy: Option<String>) -> Self {
        Self {
            proxy: proxy.filter(|p| !p.trim().is_empty()),
            ..Self::default()
        }
    }
}

/// A failed request. `Display` never includes query strings.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HttpError {
    #[error("{url} returned {status}: {body}")]
    Status { status: u16, url: String, body: String },
    #[error("{url} timed out")]
    Timeout { url: String },
    #[error("{url}: {message}")]
    Transport { url: String, message: String },
    #[error("{url}: blocked ({reason})")]
    Blocked { url: String, reason: String },
    #[error("{url}: body larger than {limit} bytes")]
    TooLarge { url: String, limit: usize },
    #[error("invalid request: {0}")]
    Invalid(String),
}

impl HttpError {
    /// The HTTP status, for [`HttpError::Status`].
    pub fn status(&self) -> Option<u16> {
        match self {
            HttpError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Classify into a [`Failure`] (Cadence's `source.Classify`).
    pub fn to_failure(&self, source: &str) -> Failure {
        match self {
            HttpError::Status { status, .. } => Failure::from_status(source, *status, self.to_string()),
            HttpError::Timeout { .. } => Failure::common(source, "timed out", Some(self.to_string())),
            HttpError::Blocked { .. } => Failure::common(source, "address not allowed", Some(self.to_string())),
            HttpError::Invalid(_) => Failure::common(source, "invalid request", Some(self.to_string())),
            _ => Failure::common(source, format!("{source} failed"), Some(self.to_string())),
        }
    }
}

/// A clonable request description, so it can be replayed on retry.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

impl Req {
    pub fn new(method: Method, url: &str) -> Result<Self, HttpError> {
        let url = Url::parse(url).map_err(|e| HttpError::Invalid(format!("{e}")))?;
        Ok(Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
        })
    }
    pub fn get(url: &str) -> Result<Self, HttpError> {
        Self::new(Method::GET, url)
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            self.headers.insert(n, v);
        }
        self
    }
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }
}

struct ProxyState {
    blocked_until_ms: AtomicU64,
}

/// An HTTP client for one source.
pub struct HttpClient {
    name: String,
    opts: HttpOptions,
    epoch: Instant,
    proxy: Option<(ProxyState, reqwest::Client)>,
    direct: reqwest::Client,
    no_redirect: reqwest::Client,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("name", &self.name)
            .field("proxy", &self.proxy.is_some())
            .finish()
    }
}

impl HttpClient {
    /// Build a client named `name` (used in logs).
    pub fn new(name: &str, opts: HttpOptions) -> Result<Self, HttpError> {
        let direct = builder(&opts, true)
            .build()
            .map_err(|e| HttpError::Invalid(e.to_string()))?;
        let no_redirect = builder(&opts, false)
            .build()
            .map_err(|e| HttpError::Invalid(e.to_string()))?;
        let proxy = match opts.proxy.as_deref().filter(|p| !p.is_empty()) {
            None => None,
            Some(p) => {
                let px = reqwest::Proxy::all(p).map_err(|e| HttpError::Invalid(format!("{name} proxy: {e}")))?;
                let client = builder(&opts, true)
                    .proxy(px)
                    .build()
                    .map_err(|e| HttpError::Invalid(e.to_string()))?;
                Some((
                    ProxyState {
                        blocked_until_ms: AtomicU64::new(0),
                    },
                    client,
                ))
            }
        };
        Ok(Self {
            name: name.to_string(),
            opts,
            epoch: Instant::now(),
            proxy,
            direct,
            no_redirect,
        })
    }

    /// A direct client with default options.
    pub fn plain(name: &str) -> Self {
        Self::new(name, HttpOptions::default()).expect("default http client builds")
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn options(&self) -> &HttpOptions {
        &self.opts
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn proxy_usable(&self) -> Option<&reqwest::Client> {
        let (state, client) = self.proxy.as_ref()?;
        (self.now_ms() >= state.blocked_until_ms.load(Ordering::Relaxed)).then_some(client)
    }

    fn mark_proxy_failed(&self, err: &str) {
        if let Some((state, _)) = &self.proxy {
            let until = self.now_ms() + PROXY_COOLDOWN.as_millis() as u64;
            state.blocked_until_ms.store(until, Ordering::Relaxed);
            tracing::warn!(http = %self.name, error = %err, cooldown_seconds = PROXY_COOLDOWN.as_secs(), "proxy failed, using direct");
        }
    }

    fn check_target(&self, url: &Url) -> Result<(), HttpError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HttpError::Blocked {
                url: safe_url(url),
                reason: "only http and https are allowed".into(),
            });
        }
        if self.opts.block_private && literal_is_private(url) {
            return Err(HttpError::Blocked {
                url: safe_url(url),
                reason: "private address".into(),
            });
        }
        Ok(())
    }

    fn build(&self, client: &reqwest::Client, req: &Req, timeout: Option<Duration>) -> reqwest::Request {
        let mut b = client
            .request(req.method.clone(), req.url.clone())
            .headers(req.headers.clone());
        if !req.headers.contains_key(USER_AGENT) {
            b = b.header(USER_AGENT, &self.opts.user_agent);
        }
        if let Some(body) = &req.body {
            b = b.body(body.clone());
        }
        if let Some(t) = timeout {
            b = b.timeout(t);
        }
        b.build().expect("request from a parsed url builds")
    }

    /// One attempt on `client`. Like Cadence's `ResponseHeaderTimeout`, every
    /// attempt waits at most `header_timeout` for the response headers.
    async fn run(&self, client: &reqwest::Client, req: &Req, timeout: Option<Duration>) -> Result<Response, Attempt> {
        let fut = client.execute(self.build(client, req, timeout));
        match tokio::time::timeout(self.opts.header_timeout, fut).await {
            Ok(r) => r.map_err(Attempt::Failed),
            Err(_) => Err(Attempt::NoHeaders),
        }
    }

    /// Send `req`, with proxy fallback and the dead-connection retry. The
    /// status is not checked. `timeout` bounds each attempt's whole exchange;
    /// headers must arrive within `header_timeout` on every attempt.
    pub async fn execute(&self, req: &Req, timeout: Option<Duration>) -> Result<Response, HttpError> {
        self.execute_with(req, timeout, true).await
    }

    async fn execute_with(&self, req: &Req, timeout: Option<Duration>, follow: bool) -> Result<Response, HttpError> {
        self.check_target(&req.url)?;
        if let Some(px) = self.proxy_usable().filter(|_| follow) {
            match self.run(px, req, timeout).await {
                Ok(res) => return Ok(res),
                // The request's own deadline ran out: going direct would
                // only overrun it (Cadence returns on a done context).
                Err(Attempt::Failed(e)) if e.is_timeout() && timeout.is_some() => {
                    return Err(self.convert(&req.url, e));
                }
                // A proxy that accepts but never answers is a failed proxy.
                Err(e) => self.mark_proxy_failed(&e.describe(self.opts.header_timeout)),
            }
        }
        let client = if follow { &self.direct } else { &self.no_redirect };
        match self.run(client, req, timeout).await {
            Ok(res) => Ok(res),
            Err(e) if e.retryable() => {
                tracing::warn!(http = %self.name, error = %e.describe(self.opts.header_timeout), "request failed on a pooled connection, retrying on a fresh one");
                self.run(client, req, timeout)
                    .await
                    .map_err(|e| self.convert_attempt(&req.url, e))
            }
            Err(e) => Err(self.convert_attempt(&req.url, e)),
        }
    }

    fn convert_attempt(&self, url: &Url, e: Attempt) -> HttpError {
        match e {
            Attempt::NoHeaders => HttpError::Timeout { url: safe_url(url) },
            Attempt::Failed(e) => self.convert(url, e),
        }
    }

    fn convert(&self, url: &Url, e: reqwest::Error) -> HttpError {
        let url = e.url().map(safe_url).unwrap_or_else(|| safe_url(url));
        if e.is_timeout() {
            return HttpError::Timeout { url };
        }
        if e.is_redirect() {
            return HttpError::Blocked {
                url,
                reason: describe(&e),
            };
        }
        let message = describe(&e);
        if message.contains("blocked address") {
            return HttpError::Blocked {
                url,
                reason: "private address".into(),
            };
        }
        HttpError::Transport { url, message }
    }

    /// Send `req` and read the whole body (bounded by [`MAX_BODY`]). A status
    /// of 300 or more is an [`HttpError::Status`] with a body snippet.
    pub async fn fetch(&self, req: &Req) -> Result<Bytes, HttpError> {
        let res = self.execute(req, Some(self.opts.request_timeout)).await?;
        let status = res.status().as_u16();
        let url = safe_url(&req.url);
        let body = read_limited(res, MAX_BODY, &url).await?;
        if status >= 300 {
            let mut snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
            snippet.truncate(300);
            return Err(HttpError::Status {
                status,
                url,
                body: snippet,
            });
        }
        Ok(body)
    }

    /// GET `url` with extra `headers` and read the body.
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Bytes, HttpError> {
        let mut req = Req::get(url)?;
        for (k, v) in headers {
            req = req.header(k, v);
        }
        self.fetch(&req).await
    }

    /// GET `url` and decode JSON.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, HttpError> {
        let body = self.get(url, headers).await?;
        serde_json::from_slice(&body).map_err(|e| HttpError::Transport {
            url: redact(url),
            message: format!("bad json: {e}"),
        })
    }

    /// Send `req` for streaming: each attempt waits at most `header_timeout`
    /// for headers, then the response is handed back with an unbounded body.
    /// A status of 300 or more is an error.
    pub async fn stream(&self, req: &Req) -> Result<Response, HttpError> {
        let res = self.execute(req, None).await?;
        let status = res.status().as_u16();
        if status >= 300 {
            return Err(HttpError::Status {
                status,
                url: safe_url(&req.url),
                body: String::new(),
            });
        }
        Ok(res)
    }

    /// GET `url` without following redirects and return its `Location`
    /// (used to expand short links).
    pub async fn location(&self, url: &str) -> Result<Option<String>, HttpError> {
        let req = Req::get(url)?;
        let res = self.execute_with(&req, Some(Duration::from_secs(10)), false).await?;
        let loc = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(loc.map(|l| {
            Url::parse(url)
                .and_then(|base| base.join(&l))
                .map(|u| u.to_string())
                .unwrap_or(l)
        }))
    }
}

fn builder(opts: &HttpOptions, follow: bool) -> reqwest::ClientBuilder {
    let max = opts.max_redirects;
    let block = opts.block_private;
    let policy = if !follow {
        reqwest::redirect::Policy::none()
    } else {
        reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > max {
                attempt.error(format!("more than {max} redirects"))
            } else if !matches!(attempt.url().scheme(), "http" | "https") {
                attempt.error("redirect to a non-http scheme")
            } else if block && literal_is_private(attempt.url()) {
                attempt.error("redirect to a private address")
            } else {
                attempt.follow()
            }
        })
    };
    let mut b = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(opts.connect_timeout)
        .read_timeout(opts.read_timeout)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(policy);
    if block {
        b = b.dns_resolver(Arc::new(GuardedResolver));
    }
    b
}

/// DNS resolver that drops private addresses (SSRF guard).
struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = found.into_iter().filter(|a| !is_private_ip(a.ip())).collect();
            if allowed.is_empty() {
                return Err(format!("blocked address: {host} resolves only to private addresses").into());
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether `ip` is loopback, private, link-local, CGNAT, unique-local,
/// multicast, broadcast, documentation or unspecified.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
        }
    }
}

/// Whether `url`'s host is a literal private IP (or `localhost`).
pub fn literal_is_private(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => is_private_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_private_ip(IpAddr::V6(ip)),
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        None => true,
    }
}

/// Read a whole response body into memory, refusing anything past `limit`
/// (`url` is already redacted for error messages).
pub(crate) async fn read_limited(mut res: Response, limit: usize, url: &str) -> Result<Bytes, HttpError> {
    let mut buf = BytesMut::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > limit {
                    return Err(HttpError::TooLarge {
                        url: url.to_string(),
                        limit,
                    });
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(buf.freeze()),
            Err(e) if e.is_timeout() => return Err(HttpError::Timeout { url: url.to_string() }),
            Err(e) => {
                return Err(HttpError::Transport {
                    url: url.to_string(),
                    message: describe(&e),
                });
            }
        }
    }
}

/// Error message with its causes, without the URL reqwest would include.
fn describe(e: &reqwest::Error) -> String {
    let mut out = String::new();
    let mut cur: Option<&(dyn StdError + 'static)> = e.source();
    if e.is_connect() {
        out.push_str("connect error");
    } else if e.is_body() || e.is_decode() {
        out.push_str("body error");
    } else if e.is_redirect() {
        out.push_str("redirect error");
    } else {
        out.push_str("request error");
    }
    while let Some(c) = cur {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

/// Why one attempt failed.
enum Attempt {
    /// No response headers within `header_timeout`.
    NoHeaders,
    Failed(reqwest::Error),
}

impl Attempt {
    /// Worth one more try on a fresh connection: a dead pooled connection
    /// (reset, EOF) or one that silently swallowed the request (no headers),
    /// as Cadence retries "timeout awaiting response headers".
    fn retryable(&self) -> bool {
        match self {
            Attempt::NoHeaders => true,
            Attempt::Failed(e) => retryable(e),
        }
    }

    fn describe(&self, header_timeout: Duration) -> String {
        match self {
            Attempt::NoHeaders => format!("no response headers within {}ms", header_timeout.as_millis()),
            Attempt::Failed(e) => describe(e),
        }
    }
}

fn retryable(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_redirect() || e.is_builder() {
        return false;
    }
    let msg = describe(e).to_ascii_lowercase();
    [
        "connection reset",
        "connection closed",
        "broken pipe",
        "incompletemessage",
        "unexpected eof",
        "unexpected end of file",
    ]
    .iter()
    .any(|m| msg.contains(m))
}

/// `url` without credentials, query (replaced by `redacted`) or fragment.
pub fn safe_url(u: &Url) -> String {
    let mut clean = u.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    if clean.query().is_some_and(|q| !q.is_empty()) {
        clean.set_query(Some("redacted"));
    } else {
        clean.set_query(None);
    }
    clean.set_fragment(None);
    clean.to_string()
}

/// [`safe_url`] for a string; unparseable URLs become `(unparseable url)`.
pub fn redact(raw: &str) -> String {
    Url::parse(raw)
        .map(|u| safe_url(&u))
        .unwrap_or_else(|_| "(unparseable url)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    #[test]
    fn safe_url_keeps_host_and_path() {
        let u = Url::parse("https://user:pw@api.example.com/v1/tracks/5?client_id=abc#frag").unwrap();
        assert_eq!(safe_url(&u), "https://api.example.com/v1/tracks/5?redacted");
        assert_eq!(redact("::"), "(unparseable url)");
    }

    #[tokio::test]
    async fn errors_never_carry_query_strings() {
        let base = serve(Router::new().route("/tracks", get(|| async { (StatusCode::FORBIDDEN, "nope") }))).await;
        let c = HttpClient::plain("test");
        let err = c
            .get(&format!("{base}/tracks?client_id=SECRETCLIENTID&sig=SIGNED"), &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SECRETCLIENTID") && !msg.contains("SIGNED"), "{msg}");
        assert_eq!(err.status(), Some(403));
        assert_eq!(err.to_failure("x").severity, wavyzon_plugin::Severity::Suspicious);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = c
            .get(&format!("http://{addr}/x?client_id=SECRETCLIENTID"), &[])
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("SECRETCLIENTID"), "{err}");
    }

    #[tokio::test]
    async fn failed_proxy_falls_back_to_direct_and_cools_down() {
        let base = serve(Router::new().route("/ok", get(|| async { "direct" }))).await;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = listener.local_addr().unwrap();
        drop(listener);
        let c = HttpClient::new("px", HttpOptions::with_proxy(Some(format!("http://{dead}")))).unwrap();
        assert!(c.proxy_usable().is_some());
        let body = c.get(&format!("{base}/ok"), &[]).await.unwrap();
        assert_eq!(&body[..], b"direct");
        assert!(c.proxy_usable().is_none(), "proxy must be skipped during the cooldown");
    }

    /// A proxy that accepts connections but never answers is marked failed
    /// after `header_timeout` and the request goes direct.
    #[tokio::test]
    async fn stalled_proxy_falls_back_to_direct() {
        let base = serve(Router::new().route("/ok", get(|| async { "direct" }))).await;
        let stall = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = stall.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = stall.accept().await {
                held.push(sock);
            }
        });
        let c = HttpClient::new(
            "px",
            HttpOptions {
                header_timeout: Duration::from_millis(300),
                ..HttpOptions::with_proxy(Some(format!("http://{addr}")))
            },
        )
        .unwrap();
        let started = std::time::Instant::now();
        let res = c.stream(&Req::get(&format!("{base}/ok")).unwrap()).await.unwrap();
        assert_eq!(&res.bytes().await.unwrap()[..], b"direct");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        assert!(c.proxy_usable().is_none(), "stalled proxy must be put in cooldown");
    }

    /// A direct server that never sends headers is tried twice, then the
    /// request fails as a timeout.
    #[tokio::test]
    async fn header_timeout_is_retried_once_then_fails() {
        let stall = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = stall.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = accepted.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = stall.accept().await {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(sock);
            }
        });
        let c = HttpClient::new(
            "slow",
            HttpOptions {
                header_timeout: Duration::from_millis(200),
                ..Default::default()
            },
        )
        .unwrap();
        let err = c
            .stream(&Req::get(&format!("http://{addr}/x")).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Timeout { .. }), "{err:?}");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn private_addresses_are_blocked_when_asked() {
        let base = serve(Router::new().route("/x", get(|| async { "secret" }))).await;
        let c = HttpClient::new(
            "guard",
            HttpOptions {
                block_private: true,
                ..Default::default()
            },
        )
        .unwrap();
        let err = c.get(&format!("{base}/x"), &[]).await.unwrap_err();
        assert!(matches!(err, HttpError::Blocked { .. }), "{err:?}");
        let err = c.get(&base.replace("127.0.0.1", "localhost"), &[]).await.unwrap_err();
        assert!(matches!(err, HttpError::Blocked { .. }), "{err:?}");
        assert!(is_private_ip("10.1.2.3".parse().unwrap()));
        assert!(is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:192.168.0.1".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("2606:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn redirects_are_capped() {
        let app = Router::new()
            .route("/a", get(|| async { axum::response::Redirect::temporary("/b") }))
            .route("/b", get(|| async { axum::response::Redirect::temporary("/c") }))
            .route("/c", get(|| async { "end" }));
        let base = serve(app).await;
        let c = HttpClient::new(
            "r",
            HttpOptions {
                max_redirects: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(c.get(&format!("{base}/a"), &[]).await.is_err());
        let c = HttpClient::new(
            "r",
            HttpOptions {
                max_redirects: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(&c.get(&format!("{base}/a"), &[]).await.unwrap()[..], b"end");
        assert_eq!(
            c.location(&format!("{base}/a")).await.unwrap().as_deref(),
            Some(format!("{base}/b").as_str())
        );
    }
}
