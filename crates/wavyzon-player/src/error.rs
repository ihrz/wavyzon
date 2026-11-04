//! Errors returned by [`Player`](crate::Player) commands.

use wavyzon_plugin::Failure;

use crate::host::{MAX_VOICE_BYTES, MAX_VOICE_MS};

/// Why a player command was refused. Every variant except
/// [`PlayerError::Resolve`] and [`PlayerError::Destroyed`] is a client error
/// (HTTP 400/409 territory).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlayerError {
    /// A crossfade or host transition needs a track loaded on deck B first.
    #[error("no deck b is loaded")]
    NoDeckB,
    /// Seeking or mixing needs something playing on deck A.
    #[error("nothing is playing on deck a")]
    NothingToMix,
    /// `set_next` needs a current (or loading) track to queue behind.
    #[error("nothing is playing to queue a next track behind")]
    NothingPlaying,
    /// A newer `set_next`/`clear_next` (or a new track) replaced this request.
    #[error("a newer next-track request or a new track replaced this one")]
    NextSuperseded,
    /// A host transition was requested while deck A is still resolving.
    #[error("a track is still loading on deck a")]
    TrackLoading,
    /// The crossfade curve name is unknown.
    #[error("unknown curve {0:?}")]
    InvalidCurve(String),
    /// `durationMs` was zero or negative.
    #[error("durationMs must be positive")]
    InvalidDuration,
    /// The host voice clip exceeds [`MAX_VOICE_BYTES`].
    #[error("voice clip is larger than {MAX_VOICE_BYTES} bytes")]
    VoiceTooLarge,
    /// The host voice clip exceeds [`MAX_VOICE_MS`].
    #[error("voice clip is longer than {MAX_VOICE_MS}ms")]
    VoiceTooLong,
    /// The host voice clip decoded to no audio.
    #[error("voice clip has no audio")]
    VoiceEmpty,
    /// The host voice clip is not base64 MP3.
    #[error("voice clip: {0}")]
    VoiceInvalid(String),
    /// `pluginFilters` names a filter no plugin registered.
    #[error("unknown plugin filter {0:?}")]
    UnknownFilter(String),
    /// A plugin filter factory rejected its configuration.
    #[error("plugin filter {name:?}: {failure}")]
    FilterConfig {
        /// The `pluginFilters` key.
        name: String,
        /// What the factory returned.
        failure: Failure,
    },
    /// Resolving the track to something playable failed.
    #[error("{0}")]
    Resolve(Failure),
    /// The player was destroyed while the command ran.
    #[error("player destroyed")]
    Destroyed,
}
