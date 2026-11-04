//! A [`Source`] that synthesises sine tones, streamed as raw 48 kHz PCM.
//!
//! Identifiers (`loadtracks` with the `tone:` prefix, or a `tone://` link):
//!
//! | Query | Track |
//! | --- | --- |
//! | `tone:440` | 440 Hz for `DEFAULT_SECONDS` (10 s) |
//! | `tone:440:30` | 440 Hz for 30 s |
//! | `tone:440:live` | 440 Hz, endless (`isStream: true`) |
//! | `tone://1000:5` | the same, as a link |

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use wavyzon_plugin::{
    Failure, Format, LoadResult, Playable, PluginContext, PluginError, SAMPLE_RATE, Source, Stream, Track, async_trait,
};

use crate::Shared;

const SOURCE: &str = "tone";
const MIN_HZ: f64 = 20.0;
const MAX_HZ: f64 = 20_000.0;
/// Bytes per stereo sample frame: 2 channels × 2 bytes (s16le).
const BYTES_PER_FRAME: usize = 4;

/// Settings from `WAVYZON_PLUGIN_EXAMPLE_*`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneSettings {
    pub amplitude: f64,
    pub default_seconds: u32,
    pub max_seconds: u32,
}

impl Default for ToneSettings {
    fn default() -> Self {
        Self {
            amplitude: 0.2,
            default_seconds: 10,
            max_seconds: 600,
        }
    }
}

impl ToneSettings {
    pub fn from_context(ctx: &PluginContext) -> Result<Self, PluginError> {
        let mut s = Self::default();
        if let Some(v) = ctx.setting("amplitude") {
            s.amplitude = v
                .parse()
                .map_err(|_| PluginError::Config(format!("AMPLITUDE: not a number: {v}")))?;
            if !(s.amplitude > 0.0 && s.amplitude <= 1.0) {
                return Err(PluginError::Config("AMPLITUDE must be in (0, 1]".into()));
            }
        }
        if let Some(v) = ctx.setting("max_seconds") {
            s.max_seconds = v
                .parse()
                .map_err(|_| PluginError::Config(format!("MAX_SECONDS: not an integer: {v}")))?;
        }
        if let Some(v) = ctx.setting("default_seconds") {
            s.default_seconds = v
                .parse()
                .map_err(|_| PluginError::Config(format!("DEFAULT_SECONDS: not an integer: {v}")))?;
        }
        if s.default_seconds == 0 || s.default_seconds > s.max_seconds {
            return Err(PluginError::Config(
                "DEFAULT_SECONDS must be between 1 and MAX_SECONDS".into(),
            ));
        }
        Ok(s)
    }
}

/// A parsed tone identifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneSpec {
    pub hz: f64,
    /// `None` for an endless tone.
    pub seconds: Option<u32>,
}

impl ToneSpec {
    /// Parse `440`, `440:30` or `440:live`.
    pub fn parse(raw: &str, settings: &ToneSettings) -> Result<Self, Failure> {
        let bad = |why: &str| Failure::common(SOURCE, "invalid tone", Some(format!("{raw:?}: {why}")));
        let mut parts = raw.trim().splitn(2, ':');
        let hz: f64 = parts
            .next()
            .unwrap_or_default()
            .parse()
            .map_err(|_| bad("frequency is not a number"))?;
        if !(MIN_HZ..=MAX_HZ).contains(&hz) {
            return Err(bad("frequency must be between 20 and 20000 Hz"));
        }
        let seconds = match parts.next() {
            None | Some("") => Some(settings.default_seconds),
            Some("live") => None,
            Some(s) => {
                let n: u32 = s.parse().map_err(|_| bad("length must be seconds or `live`"))?;
                if n == 0 || n > settings.max_seconds {
                    return Err(bad("length is out of range"));
                }
                Some(n)
            }
        };
        Ok(Self { hz, seconds })
    }

    /// The canonical identifier stored in [`Track::identifier`].
    pub fn identifier(&self) -> String {
        match self.seconds {
            Some(s) => format!("{}:{s}", self.hz),
            None => format!("{}:live", self.hz),
        }
    }

    pub fn to_track(self) -> Track {
        let id = self.identifier();
        Track {
            source: SOURCE.into(),
            title: format!("Sine {} Hz", self.hz),
            author: "wavyzon-plugin-example".into(),
            duration_ms: self.seconds.map_or(0, |s| s as i64 * 1000),
            uri: format!("tone://{id}"),
            is_stream: self.seconds.is_none(),
            identifier: id,
            ..Default::default()
        }
    }
}

/// Counters exposed by the stats route.
#[derive(Default)]
pub struct ToneStats {
    opened: AtomicU64,
}

impl ToneStats {
    pub fn opened(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }
}

pub struct ToneSource {
    settings: ToneSettings,
    shared: Arc<Shared>,
}

impl ToneSource {
    pub fn new(settings: ToneSettings, shared: Arc<Shared>) -> Self {
        Self { settings, shared }
    }
}

#[async_trait]
impl Source for ToneSource {
    fn name(&self) -> &str {
        SOURCE
    }

    /// `loadtracks?identifier=tone:440` calls [`search`](Source::search) with `"440"`.
    fn search_prefix(&self) -> Option<&str> {
        Some(SOURCE)
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        Ok(vec![ToneSpec::parse(query, &self.settings)?.to_track()])
    }

    fn matches_url(&self, url: &str) -> bool {
        url.starts_with("tone://")
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let rest = url.strip_prefix("tone://").unwrap_or(url);
        Ok(LoadResult::single(ToneSpec::parse(rest, &self.settings)?.to_track()))
    }

    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        Ok(ToneSpec::parse(identifier, &self.settings)?.to_track())
    }

    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for ToneSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let spec = ToneSpec::parse(&track.identifier, &self.settings)?;
        let start = (start_ms.max(0) * SAMPLE_RATE as i64) / 1000;
        let reader = ToneReader::new(spec, self.settings.amplitude, start);
        self.shared.tones.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Stream {
            body: Box::new(reader),
            format: Format::Pcm48,
            // We honour the seek exactly, so tell the player where we start.
            start_sample: start,
            live: spec.seconds.is_none(),
        })
    }

    /// Generated audio can start anywhere for free.
    fn seekable(&self) -> bool {
        true
    }
}

/// Generates interleaved s16le stereo PCM at 48 kHz.
pub struct ToneReader {
    step: f64,
    amplitude: f64,
    /// Next sample frame (per channel) to produce.
    pos: i64,
    /// First sample frame past the end, `None` when endless.
    end: Option<i64>,
    pending: [u8; BYTES_PER_FRAME],
    pending_at: usize,
}

impl ToneReader {
    pub fn new(spec: ToneSpec, amplitude: f64, start_sample: i64) -> Self {
        Self {
            step: std::f64::consts::TAU * spec.hz / SAMPLE_RATE as f64,
            amplitude: amplitude.clamp(0.0, 1.0),
            pos: start_sample,
            end: spec.seconds.map(|s| s as i64 * SAMPLE_RATE as i64),
            pending: [0; BYTES_PER_FRAME],
            pending_at: BYTES_PER_FRAME,
        }
    }

    fn next_frame(&mut self) -> bool {
        if self.end.is_some_and(|e| self.pos >= e) {
            return false;
        }
        let v = ((self.pos as f64 * self.step).sin() * self.amplitude * i16::MAX as f64) as i16;
        let b = v.to_le_bytes();
        self.pending = [b[0], b[1], b[0], b[1]];
        self.pending_at = 0;
        self.pos += 1;
        true
    }
}

impl AsyncRead for ToneReader {
    fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        while buf.remaining() > 0 {
            if self.pending_at == BYTES_PER_FRAME && !self.next_frame() {
                break; // EOF: return with whatever was written (possibly nothing)
            }
            let at = self.pending_at;
            let n = (BYTES_PER_FRAME - at).min(buf.remaining());
            buf.put_slice(&self.pending[at..at + n]);
            self.pending_at += n;
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn source() -> ToneSource {
        ToneSource::new(ToneSettings::default(), Arc::new(Shared::default()))
    }

    #[test]
    fn parses_identifiers() {
        let s = ToneSettings::default();
        assert_eq!(
            ToneSpec::parse("440", &s).unwrap(),
            ToneSpec {
                hz: 440.0,
                seconds: Some(10)
            }
        );
        assert_eq!(ToneSpec::parse("440:30", &s).unwrap().seconds, Some(30));
        assert_eq!(ToneSpec::parse("261.63:live", &s).unwrap().seconds, None);
        assert!(ToneSpec::parse("abc", &s).is_err());
        assert!(ToneSpec::parse("5", &s).is_err());
        assert!(ToneSpec::parse("440:0", &s).is_err());
        assert!(ToneSpec::parse("440:601", &s).is_err());
    }

    #[test]
    fn track_roundtrips_through_identifier() {
        let s = ToneSettings::default();
        let t = ToneSpec::parse("440", &s).unwrap().to_track();
        assert_eq!(t.identifier, "440:10");
        assert_eq!(t.duration_ms, 10_000);
        assert_eq!(t.uri, "tone://440:10");
        assert_eq!(ToneSpec::parse(&t.identifier, &s).unwrap().to_track(), t);
        let live = ToneSpec::parse("440:live", &s).unwrap().to_track();
        assert!(live.is_stream);
        // tracks survive the node's base64 encoding
        assert_eq!(Track::decode(&live.encode()).unwrap(), live);
    }

    #[tokio::test]
    async fn search_link_and_lookup() {
        let src = source();
        assert_eq!(src.search("880").await.unwrap()[0].identifier, "880:10");
        assert!(src.matches_url("tone://880:2"));
        assert!(!src.matches_url("https://example.com/a.mp3"));
        let r = src.load_url("tone://880:2").await.unwrap();
        assert_eq!(r.tracks[0].duration_ms, 2000);
        assert!(src.track("nope").await.is_err());
        assert!(src.playable().unwrap().seekable());
    }

    #[tokio::test]
    async fn streams_exact_length_of_pcm() {
        let src = source();
        let t = src.track("440:1").await.unwrap();
        let mut s = src.playable().unwrap().open(&t, 0).await.unwrap();
        assert_eq!(s.format, Format::Pcm48);
        assert!(!s.live);
        let mut bytes = Vec::new();
        s.body.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes.len(), 48_000 * BYTES_PER_FRAME);
        // stereo: left == right
        assert_eq!(bytes[400..402], bytes[402..404]);
        // amplitude 0.2 → peak about 6553
        let peak = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!((6400..=6554).contains(&peak), "peak {peak}");
        assert_eq!(src.shared.tones.opened(), 1);
    }

    #[tokio::test]
    async fn honours_seek_and_odd_reads() {
        let src = source();
        let t = src.track("440:1").await.unwrap();
        let mut s = src.playable().unwrap().open(&t, 500).await.unwrap();
        assert_eq!(s.start_sample, 24_000);
        // read in 3-byte chunks to cross frame boundaries
        let mut total = 0;
        let mut chunk = [0u8; 3];
        loop {
            let n = s.body.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 24_000 * BYTES_PER_FRAME);
    }

    #[tokio::test]
    async fn live_tone_never_ends() {
        let src = source();
        let t = src.track("440:live").await.unwrap();
        let mut s = src.playable().unwrap().open(&t, 0).await.unwrap();
        assert!(s.live);
        let mut buf = vec![0u8; 1 << 20];
        s.body.read_exact(&mut buf).await.unwrap();
    }
}
