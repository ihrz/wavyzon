//! # wavyzon-player
//!
//! The per-guild player of [Wavyzon](https://gitlab.com/ihrz/wavyzon): two
//! decks with armed, beat-matched crossfades, gapless next tracks, host
//! (voice-over) transitions, Lavalink-compatible filters that glide instead
//! of jumping, plugin filters, loudness normalisation, and the **wire
//! clock**.
//!
//! ## The wire clock
//!
//! A [`Player`] is a [`FrameSource`](wavyzon_audio::FrameSource): the voice
//! sender pulls one 20 ms frame per tick with `next_frame`, writes it to the
//! socket, and calls `sent` with the time it left. Positions in
//! [`State`] (`anchorMs`, `anchorSentAt`, `anchorAudibleAt`, `rate`) and
//! `TrackAudibleEvent` are stamped there, so they describe what listeners
//! hear, not where the decoder is.
//!
//! ## Pipeline of a deck
//!
//! ```text
//! Opener (async) ─► byte pump ─► decoder (blocking thread) ─► bounded chunk queue
//!   ─► resampler (rate × pitch) ─► WSOLA stretcher (speed) ─► built-in filters
//!   ─► plugin filters ─► (mix with deck B) ─► volume ─► pause envelope ─► frame
//! ```
//!
//! Only the part after the queue runs in `next_frame`; it never blocks on I/O.
//!
//! ## Events
//!
//! Every event goes to [`PlayerConfig::on_event`], in order, from a dedicated
//! task, with the player's `guildId` and the track `instance`.
//!
//! ## Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use wavyzon_player::*;
//! # async fn demo(opener: Arc<dyn Opener>, track: wavyzon_plugin::Track) {
//! let cfg = PlayerConfig::new(
//!     "1234",
//!     opener,
//!     Arc::new(IdentityResolver),
//!     Arc::new(|e| println!("{}", serde_json::to_string(&e).unwrap())),
//!     tokio::runtime::Handle::current(),
//! );
//! let player = Player::new(cfg);
//! player.play(track, 0, false);
//! // hand `player.clone()` to the voice sender as an `Arc<dyn FrameSource>`
//! # }
//! ```

mod deck;
mod error;
mod events;
mod filter_ramp;
mod filters;
mod gapless;
mod host;
mod loudness;
mod player;
mod plugin_chain;
mod reverb;
mod traits;

#[cfg(test)]
mod tests;

pub use error::PlayerError;
pub use filter_ramp::FILTER_RAMP_TAU_MS;
pub use filters::{Band, EQUALIZER_BANDS_HZ, Filters, Karaoke, LowPass, Oscillator, Reverb, Rotation, Timescale};
pub use host::{HostRequest, MAX_VOICE_BYTES, MAX_VOICE_MS};
pub use loudness::normalization_gain;
pub use player::{CrossfadeRequest, MAX_VOLUME, Player, PlayerConfig, State};
pub use plugin_chain::{FilterRegistry, filter_registry};
pub use traits::{
    EventHandler, IdentityResolver, Loudness, LoudnessLookup, Opener, Resolver, Tap, TapFactory, UnmeasuredHook,
};
pub use wavyzon_audio::dsp::{FadeCurve, TransitionProfile};
