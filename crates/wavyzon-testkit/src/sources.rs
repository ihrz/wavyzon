//! Mock [`Source`] + [`Playable`] implementations.
//!
//! | Mock | Use it to test |
//! | --- | --- |
//! | [`ToneSource`] | playback, seeking (`start_ms` → `start_sample`), positions, frequency checks |
//! | [`FailingSource`] | `loadType: "error"`, `TrackExceptionEvent` with each [`Severity`], mid-stream errors |
//! | [`SlowSource`] | load timeouts, slow first audio, `TrackStuckEvent` (stalls mid-stream) |
//! | [`ScriptedSource`] | canned search / link / track results and call recording |
//!
//! All of them stream [`Format::Pcm48`] sine tones generated on the fly, so
//! nothing touches the network or the disk.
//!
//! Tone identifiers are `"<freq_hz>:<duration_ms>"` (`"440:3000"`); a
//! duration of `0` makes an endless (live) stream.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, ReadBuf};
use wavyzon_plugin::{Failure, Format, LoadResult, Playable, Severity, Source, Stream, Track};

use crate::audio::sine_sample;

/// Samples per millisecond at 48 kHz.
const PER_MS: i64 = 48;

/// A bare track of `source` with `identifier`, a title and `duration_ms`.
pub fn test_track(source: &str, identifier: &str, duration_ms: i64) -> Track {
    Track {
        source: source.into(),
        identifier: identifier.into(),
        title: format!("Test track {identifier}"),
        author: "wavyzon-testkit".into(),
        duration_ms,
        uri: format!("{source}://{identifier}"),
        is_stream: duration_ms == 0,
        ..Default::default()
    }
}

/// Parse a tone identifier `"<freq>:<duration_ms>"` (or `"<freq>/<ms>"`, or
/// just `"<freq>"` for 30 s).
pub fn parse_tone_identifier(id: &str) -> Option<(f64, i64)> {
    let mut parts = id.splitn(2, [':', '/']);
    let freq: f64 = parts.next()?.trim().parse().ok()?;
    let ms: i64 = match parts.next() {
        Some(p) => p.trim().parse().ok()?,
        None => 30_000,
    };
    (freq > 0.0 && freq < 24_000.0 && ms >= 0).then_some((freq, ms))
}

// ----- readers ----------------------------------------------------------------

/// An [`AsyncRead`] producing a stereo 48 kHz `i16` LE sine, sample-accurate
/// from any start sample (phase is continuous across seeks).
pub struct ToneReader {
    freq: f64,
    amplitude: f64,
    next: u64,
    end: Option<u64>,
    pending: [u8; 4],
    pending_pos: usize,
}

impl ToneReader {
    /// A tone of `freq` Hz from absolute sample `start` up to `end`
    /// (`None` = endless).
    pub fn new(freq: f64, amplitude: f64, start: u64, end: Option<u64>) -> Self {
        Self {
            freq,
            amplitude,
            next: start,
            end,
            pending: [0; 4],
            pending_pos: 4,
        }
    }
}

impl AsyncRead for ToneReader {
    fn poll_read(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while buf.remaining() > 0 {
            if this.pending_pos < 4 {
                let k = buf.remaining().min(4 - this.pending_pos);
                buf.put_slice(&this.pending[this.pending_pos..this.pending_pos + k]);
                this.pending_pos += k;
                continue;
            }
            if this.end.is_some_and(|e| this.next >= e) {
                break;
            }
            let b = sine_sample(this.freq, this.amplitude, 48_000, this.next).to_le_bytes();
            this.next += 1;
            let frame = [b[0], b[1], b[0], b[1]];
            if buf.remaining() >= 4 {
                buf.put_slice(&frame);
            } else {
                this.pending = frame;
                this.pending_pos = 0;
            }
        }
        Poll::Ready(Ok(()))
    }
}

/// Wraps a reader: after `limit` bytes, fails with `error`.
struct FailAfter<R> {
    inner: R,
    remaining: usize,
    error: String,
}

impl<R: AsyncRead + Unpin> AsyncRead for FailAfter<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(Err(io::Error::other(this.error.clone())));
        }
        let max = buf.remaining().min(this.remaining);
        let mut tmp = vec![0u8; max];
        let mut sub = ReadBuf::new(&mut tmp);
        match Pin::new(&mut this.inner).poll_read(cx, &mut sub) {
            Poll::Ready(Ok(())) => {
                let n = sub.filled().len();
                buf.put_slice(sub.filled());
                this.remaining -= n;
                if n == 0 {
                    return Poll::Ready(Err(io::Error::other(this.error.clone())));
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Wraps a reader: after `limit` bytes, stops producing data for `stall`
/// (`None` = forever), then continues.
struct StallAfter<R> {
    inner: R,
    remaining: usize,
    stall: Option<Duration>,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    done_stalling: bool,
    hit: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for StallAfter<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.done_stalling && this.remaining == 0 {
            this.hit.store(true, Ordering::SeqCst);
            let Some(d) = this.stall else { return Poll::Pending };
            let sleep = this.sleep.get_or_insert_with(|| Box::pin(tokio::time::sleep(d)));
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.done_stalling = true;
        }
        if this.done_stalling {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let max = buf.remaining().min(this.remaining);
        let mut tmp = vec![0u8; max];
        let mut sub = ReadBuf::new(&mut tmp);
        let r = Pin::new(&mut this.inner).poll_read(cx, &mut sub);
        if let Poll::Ready(Ok(())) = r {
            this.remaining -= sub.filled().len();
            buf.put_slice(sub.filled());
        }
        r
    }
}

fn bytes_for_ms(ms: u64) -> usize {
    ms as usize * PER_MS as usize * 4
}

fn tone_stream(freq: f64, amplitude: f64, duration_ms: i64, start_ms: i64) -> (ToneReader, i64, bool) {
    let live = duration_ms == 0;
    let total = (duration_ms * PER_MS) as u64;
    let mut start = start_ms.max(0) * PER_MS;
    if !live {
        start = start.min(total as i64);
    }
    (
        ToneReader::new(freq, amplitude, start as u64, (!live).then_some(total)),
        start,
        live,
    )
}

// ----- ToneSource ---------------------------------------------------------------

/// A source of sine tones. Identifiers are `"<freq_hz>:<duration_ms>"`.
///
/// * `search("440:3000")` and `track("440:3000")` return a track;
/// * links `tone://440:3000` are matched;
/// * `open(track, start_ms)` streams [`Format::Pcm48`] from exactly
///   `start_ms` and sets `start_sample = start_ms * 48` (it is seekable).
///
/// ```
/// # tokio_test_block(async {
/// use wavyzon_plugin::{Format, Source};
/// use wavyzon_testkit::{ToneSource, audio};
///
/// let src = ToneSource::new();
/// let track = src.tone_track(440.0, 2_000);
/// let mut stream = src.playable().unwrap().open(&track, 500).await.unwrap();
/// assert_eq!(stream.format, Format::Pcm48);
/// assert_eq!(stream.start_sample, 24_000);
/// let pcm = audio::read_pcm48(&mut stream, 10_000).await.unwrap();
/// assert_eq!(pcm.len(), 1_500 * 48 * 2);
/// # });
/// # fn tokio_test_block<F: std::future::Future>(f: F) { tokio::runtime::Runtime::new().unwrap().block_on(f); }
/// ```
pub struct ToneSource {
    name: String,
    prefix: Option<String>,
    amplitude: f64,
    opens: Mutex<Vec<(Track, i64)>>,
}

impl Default for ToneSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ToneSource {
    /// Name `"tone"`, search prefix `"tonesearch"`, amplitude 0.5.
    pub fn new() -> Self {
        Self {
            name: "tone".into(),
            prefix: Some("tonesearch".into()),
            amplitude: 0.5,
            opens: Mutex::new(Vec::new()),
        }
    }

    /// Use another source name (several tone sources side by side).
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.into();
        self
    }

    /// Use another search prefix (`None`: the source does not search).
    pub fn with_search_prefix(mut self, prefix: Option<&str>) -> Self {
        self.prefix = prefix.map(str::to_string);
        self
    }

    /// Peak amplitude, `0.0..=1.0`.
    pub fn with_amplitude(mut self, amplitude: f64) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// The track for a tone of `freq_hz` lasting `duration_ms` (`0` = live).
    pub fn tone_track(&self, freq_hz: f64, duration_ms: i64) -> Track {
        let mut t = test_track(&self.name, &format!("{freq_hz}:{duration_ms}"), duration_ms);
        t.title = format!("Tone {freq_hz} Hz");
        t
    }

    /// Every `open` call: the track and the requested `start_ms`.
    pub fn opens(&self) -> Vec<(Track, i64)> {
        self.opens.lock().clone()
    }

    /// Number of `open` calls.
    pub fn open_count(&self) -> usize {
        self.opens.lock().len()
    }

    fn resolve(&self, id: &str) -> Result<Track, Failure> {
        let (freq, ms) = parse_tone_identifier(id)
            .ok_or_else(|| Failure::common(&self.name, "bad tone identifier", Some(id.to_string())))?;
        Ok(self.tone_track(freq, ms))
    }
}

#[async_trait]
impl Source for ToneSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn search_prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        Ok(self.resolve(query).into_iter().collect())
    }
    fn matches_url(&self, url: &str) -> bool {
        url.starts_with(&format!("{}://", self.name))
    }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let id = url.strip_prefix(&format!("{}://", self.name)).unwrap_or(url);
        self.resolve(id).map(LoadResult::single)
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        self.resolve(identifier)
    }
    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for ToneSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        self.opens.lock().push((track.clone(), start_ms));
        let (freq, ms) = parse_tone_identifier(&track.identifier)
            .ok_or_else(|| Failure::common(&self.name, "bad tone identifier", Some(track.identifier.clone())))?;
        let (reader, start_sample, live) = tone_stream(freq, self.amplitude, ms, start_ms);
        Ok(Stream {
            body: Box::new(reader),
            format: Format::Pcm48,
            start_sample,
            live,
        })
    }
    fn seekable(&self) -> bool {
        true
    }
}

// ----- FailingSource ---------------------------------------------------------------

/// Where a [`FailingSource`] fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailAt {
    /// `search`, `track` and `load_url` fail (→ `loadType: "error"`).
    Resolve,
    /// Resolving works, `open` fails (→ `TrackExceptionEvent` before audio).
    Open,
    /// The stream errors after `after_ms` of audio (→ exception mid-track).
    MidStream {
        /// Milliseconds of audio before the read error.
        after_ms: u64,
    },
}

/// A source that fails with a configurable [`Failure`] at a configurable
/// [`FailAt`] stage. Name `"failing"`, prefix `"failsearch"`, links `failing://…`.
///
/// ```
/// use wavyzon_plugin::Severity;
/// use wavyzon_testkit::{FailAt, FailingSource};
/// let src = FailingSource::new(Severity::Suspicious).at(FailAt::MidStream { after_ms: 200 }).with_message("banned");
/// ```
pub struct FailingSource {
    name: String,
    failure: Failure,
    at: FailAt,
    opens: AtomicUsize,
}

impl FailingSource {
    /// Fails at [`FailAt::Open`] with `severity` and message "scripted failure".
    pub fn new(severity: Severity) -> Self {
        Self {
            name: "failing".into(),
            failure: Failure::new(severity, "failing", "scripted failure", Some("wavyzon-testkit".into())),
            at: FailAt::Open,
            opens: AtomicUsize::new(0),
        }
    }

    /// Choose the failing stage.
    pub fn at(mut self, at: FailAt) -> Self {
        self.at = at;
        self
    }

    /// Replace the failure message.
    pub fn with_message(mut self, message: &str) -> Self {
        self.failure.message = message.into();
        self
    }

    /// Replace the whole failure.
    pub fn with_failure(mut self, failure: Failure) -> Self {
        self.failure = failure;
        self
    }

    /// Use another source name.
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.into();
        self.failure.source = name.into();
        self
    }

    /// The failure this source reports.
    pub fn failure(&self) -> &Failure {
        &self.failure
    }

    /// A track of this source (`identifier` is free-form, 30 s long).
    pub fn failing_track(&self, identifier: &str) -> Track {
        test_track(&self.name, identifier, 30_000)
    }

    /// Number of `open` calls.
    pub fn open_count(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    fn resolve(&self, id: &str) -> Result<Track, Failure> {
        if self.at == FailAt::Resolve {
            Err(self.failure.clone())
        } else {
            Ok(self.failing_track(id))
        }
    }
}

#[async_trait]
impl Source for FailingSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn search_prefix(&self) -> Option<&str> {
        Some("failsearch")
    }
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        self.resolve(query).map(|t| vec![t])
    }
    fn matches_url(&self, url: &str) -> bool {
        url.starts_with(&format!("{}://", self.name))
    }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let id = url.strip_prefix(&format!("{}://", self.name)).unwrap_or(url);
        self.resolve(id).map(LoadResult::single)
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        self.resolve(identifier)
    }
    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for FailingSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        match self.at {
            FailAt::Resolve | FailAt::Open => Err(self.failure.clone()),
            FailAt::MidStream { after_ms } => {
                let (reader, start_sample, _) = tone_stream(440.0, 0.5, track.duration_ms.max(1), start_ms);
                let body = FailAfter {
                    inner: reader,
                    remaining: bytes_for_ms(after_ms),
                    error: self.failure.to_string(),
                };
                Ok(Stream {
                    body: Box::new(body),
                    format: Format::Pcm48,
                    start_sample,
                    live: false,
                })
            }
        }
    }
    fn seekable(&self) -> bool {
        true
    }
}

// ----- SlowSource ---------------------------------------------------------------

/// A tone source that is slow on purpose: delayed resolve, delayed `open`,
/// and/or a stall after some audio (to trigger stuck detection).
/// Name `"slow"`, prefix `"slowsearch"`, tone identifiers.
///
/// ```
/// use std::time::Duration;
/// use wavyzon_testkit::SlowSource;
/// // 2 s before the first byte, then 1 s of audio, then nothing ever again.
/// let src = SlowSource::new().open_delay(Duration::from_secs(2)).stall_after(1_000, None);
/// ```
pub struct SlowSource {
    tone: ToneSource,
    resolve_delay: Duration,
    open_delay: Duration,
    stall: Option<(u64, Option<Duration>)>,
    stalled: Arc<AtomicBool>,
}

impl Default for SlowSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SlowSource {
    /// No delays yet; configure with the builder methods.
    pub fn new() -> Self {
        Self {
            tone: ToneSource::new()
                .with_name("slow")
                .with_search_prefix(Some("slowsearch")),
            resolve_delay: Duration::ZERO,
            open_delay: Duration::ZERO,
            stall: None,
            stalled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Use another source name.
    pub fn with_name(mut self, name: &str) -> Self {
        self.tone = self.tone.with_name(name);
        self
    }

    /// Delay `search`, `track` and `load_url`.
    pub fn resolve_delay(mut self, d: Duration) -> Self {
        self.resolve_delay = d;
        self
    }

    /// Delay `open` (time to first byte).
    pub fn open_delay(mut self, d: Duration) -> Self {
        self.open_delay = d;
        self
    }

    /// After `after_ms` of audio, produce nothing for `stall` (`None` =
    /// forever), then continue.
    pub fn stall_after(mut self, after_ms: u64, stall: Option<Duration>) -> Self {
        self.stall = Some((after_ms, stall));
        self
    }

    /// A track of this source.
    pub fn tone_track(&self, freq_hz: f64, duration_ms: i64) -> Track {
        self.tone.tone_track(freq_hz, duration_ms)
    }

    /// Whether a stream reached its stall point.
    pub fn stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
    }

    /// Number of `open` calls.
    pub fn open_count(&self) -> usize {
        self.tone.open_count()
    }
}

#[async_trait]
impl Source for SlowSource {
    fn name(&self) -> &str {
        self.tone.name()
    }
    fn search_prefix(&self) -> Option<&str> {
        self.tone.search_prefix()
    }
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        tokio::time::sleep(self.resolve_delay).await;
        self.tone.search(query).await
    }
    fn matches_url(&self, url: &str) -> bool {
        self.tone.matches_url(url)
    }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        tokio::time::sleep(self.resolve_delay).await;
        self.tone.load_url(url).await
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        tokio::time::sleep(self.resolve_delay).await;
        Source::track(&self.tone, identifier).await
    }
    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for SlowSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        tokio::time::sleep(self.open_delay).await;
        let mut s = self.tone.open(track, start_ms).await?;
        if let Some((after_ms, stall)) = self.stall {
            s.body = Box::new(StallAfter {
                inner: s.body,
                remaining: bytes_for_ms(after_ms),
                stall,
                sleep: None,
                done_stalling: false,
                hit: self.stalled.clone(),
            });
        }
        Ok(s)
    }
    fn seekable(&self) -> bool {
        true
    }
}

// ----- ScriptedSource ---------------------------------------------------------------

/// A call made to a [`ScriptedSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCall {
    /// `search(query)`.
    Search(String),
    /// `load_url(url)`.
    LoadUrl(String),
    /// `track(identifier)`.
    Track(String),
    /// `open(track, start_ms)`.
    Open {
        /// The opened track's identifier.
        identifier: String,
        /// Requested start.
        start_ms: i64,
    },
    /// `warm(track)`.
    Warm(String),
}

#[derive(Default)]
struct Script {
    searches: HashMap<String, Result<Vec<Track>, Failure>>,
    urls: HashMap<String, Result<LoadResult, Failure>>,
    tracks: HashMap<String, Result<Track, Failure>>,
}

/// A source answering from a script of canned results, recording every
/// call. Unscripted searches return no results; unscripted tracks and links
/// fail with a `common` failure. When playable (the default) it streams a
/// tone (440 Hz) as long as the track's `duration_ms` (`0` = live).
///
/// ```
/// use wavyzon_plugin::LoadResult;
/// use wavyzon_testkit::ScriptedSource;
/// use wavyzon_testkit::sources::test_track;
///
/// let a = test_track("scripted", "a", 1_000);
/// let src = ScriptedSource::new("scripted")
///     .with_search_prefix("scsearch")
///     .matching_urls("https://scripted.test/")
///     .on_search("hello", Ok(vec![a.clone()]))
///     .on_url("https://scripted.test/a", Ok(LoadResult::single(a.clone())))
///     .on_track("a", Ok(a));
/// ```
pub struct ScriptedSource {
    name: String,
    prefix: Option<String>,
    url_prefix: Option<String>,
    playable: bool,
    tone_hz: f64,
    script: Mutex<Script>,
    calls: Mutex<Vec<SourceCall>>,
}

impl ScriptedSource {
    /// An empty script for a source called `name`.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            prefix: None,
            url_prefix: None,
            playable: true,
            tone_hz: 440.0,
            script: Mutex::new(Script::default()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Answer searches under `prefix` (`"scsearch"`).
    pub fn with_search_prefix(mut self, prefix: &str) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Claim links starting with `prefix`.
    pub fn matching_urls(mut self, prefix: &str) -> Self {
        self.url_prefix = Some(prefix.into());
        self
    }

    /// No playback capability (`playable()` returns `None`).
    pub fn not_playable(mut self) -> Self {
        self.playable = false;
        self
    }

    /// Frequency of the streamed tone.
    pub fn with_tone(mut self, hz: f64) -> Self {
        self.tone_hz = hz;
        self
    }

    /// Script `search(query)`.
    pub fn on_search(self, query: &str, result: Result<Vec<Track>, Failure>) -> Self {
        self.set_search(query, result);
        self
    }

    /// Script `load_url(url)`.
    pub fn on_url(self, url: &str, result: Result<LoadResult, Failure>) -> Self {
        self.set_url(url, result);
        self
    }

    /// Script `track(identifier)`.
    pub fn on_track(self, identifier: &str, result: Result<Track, Failure>) -> Self {
        self.set_track(identifier, result);
        self
    }

    /// Script `search(query)` on a shared source.
    pub fn set_search(&self, query: &str, result: Result<Vec<Track>, Failure>) {
        self.script.lock().searches.insert(query.into(), result);
    }

    /// Script `load_url(url)` on a shared source.
    pub fn set_url(&self, url: &str, result: Result<LoadResult, Failure>) {
        self.script.lock().urls.insert(url.into(), result);
    }

    /// Script `track(identifier)` on a shared source.
    pub fn set_track(&self, identifier: &str, result: Result<Track, Failure>) {
        self.script.lock().tracks.insert(identifier.into(), result);
    }

    /// A track owned by this source.
    pub fn make_track(&self, identifier: &str, duration_ms: i64) -> Track {
        test_track(&self.name, identifier, duration_ms)
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<SourceCall> {
        self.calls.lock().clone()
    }

    fn log(&self, c: SourceCall) {
        self.calls.lock().push(c);
    }
}

#[async_trait]
impl Source for ScriptedSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn search_prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        self.log(SourceCall::Search(query.into()));
        self.script
            .lock()
            .searches
            .get(query)
            .cloned()
            .unwrap_or_else(|| Ok(vec![]))
    }
    fn matches_url(&self, url: &str) -> bool {
        self.url_prefix.as_ref().is_some_and(|p| url.starts_with(p.as_str()))
    }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        self.log(SourceCall::LoadUrl(url.into()));
        self.script
            .lock()
            .urls
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(Failure::common(&self.name, "unscripted link", Some(url.into()))))
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        self.log(SourceCall::Track(identifier.into()));
        self.script
            .lock()
            .tracks
            .get(identifier)
            .cloned()
            .unwrap_or_else(|| Err(Failure::common(&self.name, "unscripted track", Some(identifier.into()))))
    }
    fn playable(&self) -> Option<&dyn Playable> {
        if self.playable { Some(self) } else { None }
    }
}

#[async_trait]
impl Playable for ScriptedSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        self.log(SourceCall::Open {
            identifier: track.identifier.clone(),
            start_ms,
        });
        let (reader, start_sample, live) = tone_stream(self.tone_hz, 0.5, track.duration_ms, start_ms);
        Ok(Stream {
            body: Box::new(reader),
            format: Format::Pcm48,
            start_sample,
            live,
        })
    }
    fn seekable(&self) -> bool {
        true
    }
    async fn warm(&self, track: &Track) {
        self.log(SourceCall::Warm(track.identifier.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{self, assert_frequency, channel};
    use tokio::io::AsyncReadExt;

    #[test]
    fn identifiers() {
        assert_eq!(parse_tone_identifier("440:3000"), Some((440.0, 3000)));
        assert_eq!(parse_tone_identifier("1000/0"), Some((1000.0, 0)));
        assert_eq!(parse_tone_identifier("220"), Some((220.0, 30_000)));
        assert_eq!(parse_tone_identifier("x:1"), None);
        assert_eq!(parse_tone_identifier("-1:1"), None);
    }

    #[tokio::test]
    async fn tone_source_resolves_and_seeks() {
        let src = ToneSource::new();
        assert_eq!(src.search("440:1000").await.unwrap()[0].duration_ms, 1000);
        assert!(src.search("nope").await.unwrap().is_empty());
        assert!(src.matches_url("tone://440:1000"));
        let lr = src.load_url("tone://440:1000").await.unwrap();
        assert_eq!(lr.tracks[0].identifier, "440:1000");
        let t = Source::track(&src, "880:1000").await.unwrap();
        assert_eq!(t.source, "tone");

        let mut s = src.open(&t, 250).await.unwrap();
        assert_eq!(s.start_sample, 12_000);
        assert!(!s.live);
        let pcm = audio::read_pcm48(&mut s, 5_000).await.unwrap();
        assert_eq!(pcm.len(), 750 * 48 * 2);
        assert_frequency(&channel(&pcm, 2, 0), 48_000, 880.0, 2.0);
        // Phase continuity: the seeked stream equals the tail of a full one.
        let mut full = src.open(&t, 0).await.unwrap();
        let all = audio::read_pcm48(&mut full, 5_000).await.unwrap();
        assert_eq!(&all[12_000 * 2..], &pcm[..]);
        assert_eq!(src.open_count(), 2);
        // Seeking past the end yields an empty stream.
        let mut past = src.open(&t, 5_000).await.unwrap();
        assert!(audio::read_pcm48(&mut past, 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn odd_sized_reads_are_consistent() {
        let src = ToneSource::new();
        let t = src.tone_track(440.0, 100);
        let mut a = src.open(&t, 0).await.unwrap();
        let mut whole = Vec::new();
        a.body.read_to_end(&mut whole).await.unwrap();
        let mut b = src.open(&t, 0).await.unwrap();
        let mut pieces = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = b.body.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            pieces.extend_from_slice(&buf[..n]);
        }
        assert_eq!(whole, pieces);
        assert_eq!(whole.len(), 100 * 48 * 4);
    }

    #[tokio::test]
    async fn live_tone_is_endless() {
        let src = ToneSource::new();
        let t = src.tone_track(440.0, 0);
        assert!(t.is_stream);
        let mut s = src.open(&t, 1000).await.unwrap();
        assert!(s.live);
        assert_eq!(audio::read_pcm48(&mut s, 2_000).await.unwrap().len(), 2_000 * 48 * 2);
    }

    #[tokio::test]
    async fn failing_source_stages() {
        let r = FailingSource::new(Severity::Suspicious).at(FailAt::Resolve);
        assert_eq!(r.search("x").await.unwrap_err().severity, Severity::Suspicious);
        assert!(r.load_url("failing://x").await.is_err());

        let o = FailingSource::new(Severity::Fault).with_message("boom");
        let t = Source::track(&o, "x").await.unwrap();
        let e = o.open(&t, 0).await.unwrap_err();
        assert_eq!((e.severity, e.message.as_str()), (Severity::Fault, "boom"));

        let m = FailingSource::new(Severity::Common).at(FailAt::MidStream { after_ms: 100 });
        let mut s = m.open(&m.failing_track("y"), 0).await.unwrap();
        let mut got = Vec::new();
        let err = s.body.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(got.len(), bytes_for_ms(100));
        assert!(err.to_string().contains("scripted failure"));
        assert_eq!(m.open_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_source_delays_and_stalls() {
        let src = SlowSource::new()
            .open_delay(Duration::from_secs(3))
            .stall_after(100, Some(Duration::from_secs(10)));
        let t = src.tone_track(440.0, 1_000);
        let start = tokio::time::Instant::now();
        let mut s = src.open(&t, 0).await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(3));
        let mut first = vec![0u8; bytes_for_ms(100)];
        s.body.read_exact(&mut first).await.unwrap();
        assert!(!src.stalled());
        let before = tokio::time::Instant::now();
        let mut rest = Vec::new();
        s.body.read_to_end(&mut rest).await.unwrap();
        assert!(src.stalled());
        assert!(before.elapsed() >= Duration::from_secs(10));
        assert_eq!(rest.len(), bytes_for_ms(900));
    }

    #[tokio::test]
    async fn slow_source_forever_stall() {
        let src = SlowSource::new().stall_after(20, None);
        let mut s = src.open(&src.tone_track(440.0, 1_000), 0).await.unwrap();
        let mut buf = vec![0u8; bytes_for_ms(20)];
        s.body.read_exact(&mut buf).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(100), s.body.read(&mut buf)).await;
        assert!(r.is_err(), "stream should stall forever");
        assert!(src.stalled());
    }

    #[tokio::test]
    async fn scripted_source() {
        let a = test_track("scripted", "a", 500);
        let src = ScriptedSource::new("scripted")
            .with_search_prefix("scsearch")
            .matching_urls("https://s.test/")
            .on_search("q", Ok(vec![a.clone()]))
            .on_track("a", Ok(a.clone()));
        src.set_url("https://s.test/a", Err(Failure::suspicious("scripted", "nope", None)));
        assert_eq!(src.search("q").await.unwrap(), vec![a.clone()]);
        assert!(src.search("other").await.unwrap().is_empty());
        assert!(src.matches_url("https://s.test/a") && !src.matches_url("https://x.test/"));
        assert_eq!(
            src.load_url("https://s.test/a").await.unwrap_err().severity,
            Severity::Suspicious
        );
        assert!(Source::track(&src, "zzz").await.is_err());
        let mut s = src.playable().unwrap().open(&a, 100).await.unwrap();
        assert_eq!(audio::read_pcm48(&mut s, 10_000).await.unwrap().len(), 400 * 48 * 2);
        src.warm(&a).await;
        assert_eq!(
            src.calls(),
            vec![
                SourceCall::Search("q".into()),
                SourceCall::Search("other".into()),
                SourceCall::LoadUrl("https://s.test/a".into()),
                SourceCall::Track("zzz".into()),
                SourceCall::Open {
                    identifier: "a".into(),
                    start_ms: 100
                },
                SourceCall::Warm("a".into()),
            ]
        );
        assert!(ScriptedSource::new("x").not_playable().playable().is_none());
    }
}
