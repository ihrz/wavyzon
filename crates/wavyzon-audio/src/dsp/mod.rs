//! DSP building blocks shared by the player: filters, the resampler and
//! time stretcher, crossfade curves, transition profiles and host shapes.

mod biquad;
mod crossfade;
mod deck_dsp;
mod host;
mod mix;
mod profile;
mod resampler;
mod stretch;

pub use biquad::Biquad;
pub use crossfade::{Crossfade, FadeCurve, UnknownCurve, parse_curve};
pub use deck_dsp::{BASS_HZ, DeckDsp, MIN_SWEEP_HZ};
pub use host::{Bus, HostShape, duck_gain};
pub use mix::{mix, ramp, scale};
pub use profile::TransitionProfile;
pub use resampler::Resampler;
pub use stretch::{STRETCH_HOP, Stretcher};

#[cfg(test)]
pub(crate) mod test_util {
    /// Interleaved stereo sine at `hz`, amplitude 10000.
    pub fn sine(rate: f64, hz: f64, seconds: f64) -> Vec<i16> {
        let n = (rate * seconds) as usize;
        let mut out = vec![0i16; 2 * n];
        for i in 0..n {
            let v = (10000.0 * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin()) as i16;
            out[2 * i] = v;
            out[2 * i + 1] = v;
        }
        out
    }

    /// Sign changes of the left channel.
    pub fn zero_crossings(samples: &[f64]) -> usize {
        let mut n = 0;
        let mut i = 2;
        while i < samples.len() {
            if (samples[i - 2] < 0.0) != (samples[i] < 0.0) {
                n += 1;
            }
            i += 2;
        }
        n
    }
}
