//! Source traits. A source resolves queries and links to [`Track`]s; a
//! [`Playable`] source can also open an audio stream for them.

use std::fmt;

use async_trait::async_trait;
use tokio::io::AsyncRead;

use crate::{Failure, LoadResult, Track};

/// How the bytes of a [`Stream`] are encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    /// Raw interleaved signed 16-bit little-endian stereo PCM at 48 kHz.
    Pcm48,
    /// Any container/codec the decoder can probe (MP3, AAC/M4A, FLAC, Ogg
    /// Vorbis, WAV…). `hint` is a file extension or MIME type that speeds up
    /// probing (`"mp3"`, `"audio/ogg"`); it may be `None`.
    Encoded { hint: Option<String> },
}

impl Format {
    pub fn mp3() -> Self {
        Format::Encoded {
            hint: Some("mp3".into()),
        }
    }
}

/// An open audio stream.
pub struct Stream {
    pub body: Box<dyn AsyncRead + Send + Unpin>,
    pub format: Format,
    /// The 48 kHz sample (per channel) where `body` starts. A source that
    /// honoured a seek in [`Playable::open`] sets this to the real start; a
    /// source that ignored it leaves `0` and the player skips forward itself.
    pub start_sample: i64,
    /// Whether the stream has no end (internet radio, live).
    pub live: bool,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("format", &self.format)
            .field("start_sample", &self.start_sample)
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

/// Resolves searches, links and identifiers to tracks.
#[async_trait]
pub trait Source: Send + Sync + 'static {
    /// Unique name, stored in [`Track::source`] (`"soundcloud"`).
    fn name(&self) -> &str;

    /// Search prefix without the colon (`"scsearch"`), if the source searches.
    fn search_prefix(&self) -> Option<&str> {
        None
    }

    /// Plain-text search.
    async fn search(&self, _query: &str) -> Result<Vec<Track>, Failure> {
        Ok(vec![])
    }

    /// Whether [`search`](Source::search) answers free text. `false` for a
    /// source whose search only decodes an identifier (`tone:440`): the
    /// resolver then never asks it to stand in for a text search that
    /// another source found nothing for.
    fn searches_text(&self) -> bool {
        true
    }

    /// Whether this source handles `url`.
    fn matches_url(&self, _url: &str) -> bool {
        false
    }

    /// Resolve a link (track, playlist, album…).
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        Err(Failure::common(
            self.name(),
            "links are not supported",
            Some(url.to_string()),
        ))
    }

    /// Fetch one track by identifier.
    async fn track(&self, identifier: &str) -> Result<Track, Failure>;

    /// The playback capability, if this source can stream audio.
    fn playable(&self) -> Option<&dyn Playable> {
        None
    }

    /// ISRC lookup capability, used to mirror metadata-only sources.
    fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
        None
    }

    /// Recommendation capability (radio/autoplay).
    fn recommender(&self) -> Option<&dyn Recommender> {
        None
    }
}

/// A source that can stream audio.
#[async_trait]
pub trait Playable: Send + Sync {
    /// Open `track`, ideally starting at `start_ms` (see [`Stream::start_sample`]).
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure>;

    /// Whether `open` honours `start_ms` without downloading from the start.
    fn seekable(&self) -> bool {
        false
    }

    /// Hint that `track` will probably play soon (resolve URLs, warm caches).
    async fn warm(&self, _track: &Track) {}
}

#[async_trait]
pub trait IsrcLookup: Send + Sync {
    async fn by_isrc(&self, isrc: &str) -> Result<Track, Failure>;
}

#[async_trait]
pub trait Recommender: Send + Sync {
    async fn recommend(&self, identifier: &str, limit: usize) -> Result<Vec<Track>, Failure>;
}
