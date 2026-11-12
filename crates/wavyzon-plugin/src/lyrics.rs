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
