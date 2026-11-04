use super::CodecError;

pub const MAX_OPUS_BYTES: usize = 4000;
pub const DEFAULT_BITRATE: i32 = 96_000;
pub const MIN_BITRATE: i32 = 16_000;
pub const MAX_BITRATE: i32 = 384_000;
pub const DEFAULT_COMPLEXITY: u8 = 10;

/// An Opus "silence" frame, sent as a trailer when audio stops.
pub const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Clamp a requested bitrate into the range Discord accepts; `<= 0` → default.
pub fn clamp_bitrate(bps: i32) -> i32 {
    if bps <= 0 {
        DEFAULT_BITRATE
    } else {
        bps.clamp(MIN_BITRATE, MAX_BITRATE)
    }
}

/// A stereo 48 kHz Opus encoder for 20 ms frames.
pub trait Encoder: Send {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError>;
    fn set_bitrate(&mut self, bps: i32) -> Result<(), CodecError>;
}

/// Whether this build can encode Opus.
pub fn opus_available() -> bool {
    cfg!(feature = "opus")
}

#[cfg(feature = "opus")]
mod native {
    use audiopus::coder::Encoder as Inner;
    use audiopus::{Application, Bitrate, Channels, SampleRate};

    use super::*;

    pub struct Native(Inner);

    // libopus encoders hold no thread-affine state.
    unsafe impl Send for Native {}

    pub fn new(bitrate: i32) -> Result<Native, CodecError> {
        let mut enc = Inner::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|e| CodecError::Opus(e.to_string()))?;
        enc.set_complexity(DEFAULT_COMPLEXITY)
            .map_err(|e| CodecError::Opus(e.to_string()))?;
        enc.set_bitrate(Bitrate::BitsPerSecond(clamp_bitrate(bitrate)))
            .map_err(|e| CodecError::Opus(e.to_string()))?;
        Ok(Native(enc))
    }

    impl Encoder for Native {
        fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
            self.0.encode(pcm, out).map_err(|e| CodecError::Opus(e.to_string()))
        }
        fn set_bitrate(&mut self, bps: i32) -> Result<(), CodecError> {
            self.0
                .set_bitrate(Bitrate::BitsPerSecond(clamp_bitrate(bps)))
                .map_err(|e| CodecError::Opus(e.to_string()))
        }
    }
}

/// Create an encoder at `bitrate` (clamped).
pub fn new_encoder(bitrate: i32) -> Result<Box<dyn Encoder>, CodecError> {
    #[cfg(feature = "opus")]
    {
        Ok(Box::new(native::new(bitrate)?))
    }
    #[cfg(not(feature = "opus"))]
    {
        let _ = bitrate;
        Err(CodecError::NoOpus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps() {
        assert_eq!(clamp_bitrate(0), DEFAULT_BITRATE);
        assert_eq!(clamp_bitrate(1), MIN_BITRATE);
        assert_eq!(clamp_bitrate(10_000_000), MAX_BITRATE);
        assert_eq!(clamp_bitrate(128_000), 128_000);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn encodes_a_frame() {
        let mut enc = new_encoder(96_000).unwrap();
        let pcm: Vec<i16> = (0..1920).map(|i| ((i as f64 * 0.05).sin() * 8000.0) as i16).collect();
        let mut out = [0u8; MAX_OPUS_BYTES];
        let n = enc.encode(&pcm, &mut out).unwrap();
        assert!(n > 0);
        enc.set_bitrate(128_000).unwrap();
    }
}
