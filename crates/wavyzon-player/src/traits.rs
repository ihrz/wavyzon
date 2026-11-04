//! What the player needs from the outside world: something that opens audio
//! streams, something that resolves tracks to playable ones, and optional
//! hooks for analysis and loudness.

use std::sync::Arc;

use async_trait::async_trait;
use wavyzon_audio::Frame;
use wavyzon_plugin::{Failure, Stream, Track};

/// Opens the audio of a (playable) track.
///
/// Called on the async runtime; the returned [`Stream::body`] is read
/// asynchronously and decoded on a blocking thread.
#[async_trait]
pub trait Opener: Send + Sync + 'static {
    /// Open `track`, ideally starting at `start_ms`. A stream that starts
    /// elsewhere reports it in [`Stream::start_sample`] and the player skips
    /// forward itself.
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure>;
}

/// Turns a requested track into the one that actually plays (for example a
/// Spotify track into its Deezer mirror). Return the track unchanged when no
/// mirroring is needed. The player gives up after 15 s.
#[async_trait]
pub trait Resolver: Send + Sync + 'static {
    /// Resolve `track` to a playable track.
    async fn playable(&self, track: &Track) -> Result<Track, Failure>;
}

/// A [`Resolver`] that plays every track as is.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityResolver;

#[async_trait]
impl Resolver for IdentityResolver {
    async fn playable(&self, track: &Track) -> Result<Track, Failure> {
        Ok(track.clone())
    }
}

/// Receives every decoded, filtered frame of a deck that started from the
/// beginning (used for offline analysis while a track plays).
///
/// [`feed`](Tap::feed) runs on the real-time send path: it must be cheap and
/// never block.
pub trait Tap: Send + 'static {
    /// One 20 ms frame.
    fn feed(&mut self, frame: &Frame);
    /// The track played to its natural end. Not called when the deck is
    /// closed early (skip, seek, stop), so partial audio can be discarded.
    fn finish(self: Box<Self>);
}

/// Builds a [`Tap`] for a playable track, or `None` to skip it.
pub type TapFactory = Arc<dyn Fn(&Track) -> Option<Box<dyn Tap>> + Send + Sync>;

/// Measured loudness of a track (from offline analysis).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Loudness {
    /// EBU R128 integrated loudness, in LUFS.
    pub integrated_lufs: f64,
    /// Sample peak, linear `0..=1` (`0` = unknown).
    pub peak_amplitude: f64,
}

/// Looks up the loudness of `(track, playable)`; `None` when not measured.
/// Called with the player lock held: it must be a quick cache lookup.
pub type LoudnessLookup = Arc<dyn Fn(&Track, &Track) -> Option<Loudness> + Send + Sync>;

/// Called with a track that had no loudness measurement (so the integrator
/// can queue an analysis). Called with the player lock held: do not block.
pub type UnmeasuredHook = Arc<dyn Fn(&Track) + Send + Sync>;

/// Receives every player event, in order, on a dedicated task.
pub type EventHandler = Arc<dyn Fn(wavyzon_plugin::Event) + Send + Sync>;
