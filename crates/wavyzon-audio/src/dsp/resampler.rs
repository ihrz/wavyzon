//! Windowed-sinc (Kaiser) resampler that converts any input rate, optionally
//! sped up by a `factor`, to 48 kHz while tracking the source position.

use std::f64::consts::PI;

use crate::SAMPLE_RATE;

const SINC_ZERO_CROSSINGS: usize = 16;
const SINC_PHASES: usize = 256;
const KAISER_BETA: f64 = 8.6;

/// Zeroth-order modified Bessel function of the first kind.
fn bessel0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..32 {
        let h = x / (2.0 * k as f64);
        term *= h * h;
        sum += term;
    }
    sum
}

/// A polyphase table of `SINC_PHASES + 1` rows of `2 * SINC_ZERO_CROSSINGS` taps.
fn new_kernel(cutoff: f64) -> Vec<[f64; 2 * SINC_ZERO_CROSSINGS]> {
    let norm = bessel0(KAISER_BETA);
    (0..=SINC_PHASES)
        .map(|p| {
            let frac = p as f64 / SINC_PHASES as f64;
            let mut row = [0.0; 2 * SINC_ZERO_CROSSINGS];
            for (t, w_out) in row.iter_mut().enumerate() {
                let x = (t as f64 - SINC_ZERO_CROSSINGS as f64 + 1.0) - frac;
                let arg = x * cutoff;
                let s = if arg != 0.0 {
                    (PI * arg).sin() / (PI * arg) * cutoff
                } else {
                    cutoff
                };
                let w = x / SINC_ZERO_CROSSINGS as f64;
                let win = if w.abs() <= 1.0 {
                    bessel0(KAISER_BETA * (1.0 - w * w).sqrt()) / norm
                } else {
                    0.0
                };
                *w_out = s * win;
            }
            row
        })
        .collect()
}

/// Streaming stereo resampler to 48 kHz.
///
/// Write interleaved `i16` input with [`write`](Self::write), pull `f64`
/// interleaved output with [`read`](Self::read). [`position`](Self::position)
/// is the fractional input-sample index of the next output sample, which is
/// how the player keeps track of where it is in the source.
#[derive(Debug, Clone)]
pub struct Resampler {
    step: f64,
    kern: Vec<[f64; 2 * SINC_ZERO_CROSSINGS]>,
    left: Vec<f64>,
    right: Vec<f64>,
    cursor: f64,
    base: i64,
}

impl Resampler {
    /// A resampler from `in_rate`, played `factor` times faster (`1.0` = normal).
    pub fn new(in_rate: u32, factor: f64) -> Self {
        let mut r = Self {
            step: 0.0,
            kern: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
            cursor: 0.0,
            base: 0,
        };
        r.configure(in_rate, factor);
        r
    }

    /// Change the input rate or speed factor. Buffered input is kept.
    pub fn configure(&mut self, in_rate: u32, factor: f64) {
        let step = in_rate as f64 * factor / SAMPLE_RATE as f64;
        if !self.kern.is_empty() && (step - self.step).abs() < 1e-9 {
            return;
        }
        self.step = step;
        let cutoff = if step > 1.0 { 1.0 / step } else { 1.0 };
        self.kern = new_kernel(cutoff * 0.97);
    }

    /// Input samples consumed per output sample.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// Append interleaved stereo input.
    pub fn write(&mut self, interleaved: &[i16]) {
        self.left.reserve(interleaved.len() / 2);
        self.right.reserve(interleaved.len() / 2);
        for pair in interleaved.chunks_exact(2) {
            self.left.push(pair[0] as f64);
            self.right.push(pair[1] as f64);
        }
    }

    /// Input-sample index (per channel, since the last [`reset`](Self::reset)
    /// base) of the next output sample.
    pub fn position(&self) -> f64 {
        self.base as f64 + self.cursor
    }

    /// How many output frames (stereo samples) can be read right now.
    pub fn available(&self) -> usize {
        let usable = (self.left.len() as f64 - SINC_ZERO_CROSSINGS as f64) - self.cursor;
        if usable <= 0.0 {
            0
        } else {
            (usable / self.step) as usize
        }
    }

    /// Fill `out` (interleaved stereo) as far as input allows; returns the
    /// number of stereo samples written.
    pub fn read(&mut self, out: &mut [f64]) -> usize {
        let frames = out.len() / 2;
        let mut n = 0;
        while n < frames {
            let idx = self.cursor as usize;
            if idx + SINC_ZERO_CROSSINGS >= self.left.len() {
                break;
            }
            let frac = self.cursor - idx as f64;
            let row = &self.kern[(frac * SINC_PHASES as f64 + 0.5) as usize];
            let start = idx as isize - SINC_ZERO_CROSSINGS as isize + 1;
            let (mut l, mut r) = (0.0, 0.0);
            for (t, w) in row.iter().enumerate() {
                let j = start + t as isize;
                if j < 0 {
                    continue;
                }
                let j = j as usize;
                l += self.left[j] * w;
                r += self.right[j] * w;
            }
            out[2 * n] = l;
            out[2 * n + 1] = r;
            self.cursor += self.step;
            n += 1;
        }
        self.compact();
        n
    }

    fn compact(&mut self) {
        let drop = self.cursor as i64 - SINC_ZERO_CROSSINGS as i64;
        if drop < 4096 {
            return;
        }
        let drop = drop as usize;
        self.left.drain(..drop);
        self.right.drain(..drop);
        self.cursor -= drop as f64;
        self.base += drop as i64;
    }

    /// Drop buffered input and restart counting positions at `base`.
    pub fn reset(&mut self, base: i64) {
        self.left.clear();
        self.right.clear();
        self.cursor = 0.0;
        self.base = base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::test_util::{sine, zero_crossings};

    #[test]
    fn keeps_pitch_and_rate() {
        let mut r = Resampler::new(44_100, 1.0);
        r.write(&sine(44_100.0, 441.0, 1.0));
        let mut out = vec![0.0; 2 * SAMPLE_RATE as usize];
        let n = r.read(&mut out);
        assert!(
            n >= SAMPLE_RATE as usize - 200 && n <= SAMPLE_RATE as usize,
            "produced {n} frames"
        );
        let hz = zero_crossings(&out[..2 * n]) as f64 / 2.0 / (n as f64 / SAMPLE_RATE as f64);
        assert!((hz - 441.0).abs() <= 3.0, "frequency {hz}");
        assert!(
            (r.position() - n as f64 * 44_100.0 / SAMPLE_RATE as f64).abs() <= 1.0,
            "position {}",
            r.position()
        );
    }
}
