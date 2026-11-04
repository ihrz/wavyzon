//! Decoders (anything Symphonia can probe, plus raw PCM) and the Opus encoder.

mod decoder;
mod opus;

pub use decoder::{PcmStream, decode};
pub use opus::{
    DEFAULT_BITRATE, DEFAULT_COMPLEXITY, Encoder, MAX_BITRATE, MAX_OPUS_BYTES, MIN_BITRATE, OPUS_SILENCE,
    clamp_bitrate, new_encoder, opus_available,
};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("unsupported or unrecognised audio: {0}")]
    Unsupported(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("opus: {0}")]
    Opus(String),
    #[error("Wavyzon was built without the `opus` feature, so it cannot encode voice")]
    NoOpus,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
