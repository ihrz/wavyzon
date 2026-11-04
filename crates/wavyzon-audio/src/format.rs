//! Fixed output format.

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_MS: i64 = 20;
pub const FRAME_SAMPLES_PER_CHANNEL: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;
pub const FRAME_SAMPLES: usize = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;
pub const FRAMES_PER_SECOND: i64 = 1000 / FRAME_MS;

/// One 20 ms frame of interleaved stereo PCM.
pub type Frame = [i16; FRAME_SAMPLES];

/// A frame of silence.
pub const SILENCE: Frame = [0; FRAME_SAMPLES];

/// Whole frames in `ms` milliseconds.
pub fn frames_for(ms: i64) -> i64 {
    ms / FRAME_MS
}

/// Milliseconds covered by `frames` frames.
pub fn ms_for(frames: i64) -> i64 {
    frames * FRAME_MS
}

/// Round and saturate to `i16`.
pub fn clip(v: f64) -> i16 {
    if v > 32767.0 {
        32767
    } else if v < -32768.0 {
        -32768
    } else if v < 0.0 {
        (v - 0.5) as i16
    } else {
        (v + 0.5) as i16
    }
}

/// Largest absolute sample value.
pub fn peak(frame: &[i16]) -> i32 {
    frame.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(FRAME_SAMPLES_PER_CHANNEL, 960);
        assert_eq!(FRAME_SAMPLES, 1920);
        assert_eq!(FRAMES_PER_SECOND, 50);
        assert_eq!(frames_for(1000), 50);
        assert_eq!(ms_for(3), 60);
    }

    #[test]
    fn clipping() {
        assert_eq!(clip(1e9), 32767);
        assert_eq!(clip(-1e9), -32768);
        assert_eq!(clip(1.5), 2);
        assert_eq!(clip(-1.5), -2);
        let mut f = SILENCE;
        f[7] = -300;
        assert_eq!(peak(&f), 300);
    }
}
