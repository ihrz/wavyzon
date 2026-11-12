//! Lyrics providers: where `/v1/lyrics` and `/v4/lyrics` get their text.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{Failure, Track};

/// Lyrics of one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    /// Plain text, `\n` line ends.
    pub text: Arc<str>,
    /// Reported as `provider` in the LavaLyrics response (`"genius.com"`).
    pub provider: &'static str,
    /// Timed lines, in order (LavaLyrics' `lines`); empty for plain lyrics.
    pub lines: Arc<[LyricsLine]>,
}

impl Lyrics {
    /// Plain lyrics, without timed lines.
    pub fn plain(text: impl Into<Arc<str>>, provider: &'static str) -> Self {
        Self {
            text: text.into(),
            provider,
            lines: Arc::from([]),
        }
    }
}

/// One timed line of synced lyrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsLine {
    /// When the line starts, in ms from the start of the track.
    pub timestamp_ms: i64,
    /// How long it lasts, in ms (until the next line; 0 when unknown).
    pub duration_ms: i64,
    pub line: Arc<str>,
}

/// A lyrics lookup. The node asks plugin providers first, in plugin order,
/// then its built-in lyrics.ovh client: the first one that finds lyrics
/// answers.
#[async_trait]
pub trait LyricsProvider: Send + Sync + 'static {
    /// Short name for logs (`"genius"`).
    fn name(&self) -> &str;

    /// Lyrics of `track` (usually from its `author` and `title`). `Ok(None)`
    /// when the provider has none: the next provider is asked. An error is
    /// logged and counted, and the next provider is asked too.
    async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure>;
}
