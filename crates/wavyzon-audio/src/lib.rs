//! # wavyzon-audio
//!
//! The PCM format every Wavyzon stage agrees on (48 kHz, stereo, 20 ms
//! frames of `i16`), the decoders and the Opus encoder, and the hand-off
//! trait between the player and the voice sender ([`FrameSource`]), plus the
//! DSP building blocks in [`dsp`].

pub mod codec;
pub mod dsp;
mod format;
mod frame_source;

pub use format::*;
pub use frame_source::{FrameInfo, FrameSource};
