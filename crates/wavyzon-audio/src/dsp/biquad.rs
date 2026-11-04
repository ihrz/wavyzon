//! A direct-form-I biquad filter (RBJ cookbook low/high-pass).

use std::f64::consts::PI;

/// Butterworth Q, kept at Cadence's exact value so coefficients match it.
#[allow(clippy::approx_constant)]
const BUTTERWORTH_Q: f64 = 0.7071;

/// A second-order IIR section. Starts as an identity filter (`b0 = 1`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new()
    }
}

/// Keep a cutoff inside `20 Hz ..= nyquist - 100 Hz`.
fn clamp_cutoff(hz: f64, sample_rate: u32) -> f64 {
    hz.min(sample_rate as f64 / 2.0 - 100.0).max(20.0)
}

impl Biquad {
    /// An identity filter (passes input through unchanged).
    pub const fn new() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Configure as a Butterworth low-pass at `cutoff_hz`. State is kept.
    pub fn low_pass(&mut self, cutoff_hz: f64, sample_rate: u32) {
        let w0 = 2.0 * PI * clamp_cutoff(cutoff_hz, sample_rate) / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * BUTTERWORTH_Q);
        let a0 = 1.0 + alpha;
        self.b0 = ((1.0 - cos) / 2.0) / a0;
        self.b1 = (1.0 - cos) / a0;
        self.b2 = self.b0;
        self.a1 = (-2.0 * cos) / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    /// Configure as a Butterworth high-pass at `cutoff_hz`. State is kept.
    pub fn high_pass(&mut self, cutoff_hz: f64, sample_rate: u32) {
        let w0 = 2.0 * PI * clamp_cutoff(cutoff_hz, sample_rate) / sample_rate as f64;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * BUTTERWORTH_Q);
        let a0 = 1.0 + alpha;
        self.b0 = ((1.0 + cos) / 2.0) / a0;
        self.b1 = -(1.0 + cos) / a0;
        self.b2 = self.b0;
        self.a1 = (-2.0 * cos) / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    /// Set raw, already normalised (`a0 = 1`) coefficients.
    pub fn coefficients(&mut self, b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) {
        (self.b0, self.b1, self.b2, self.a1, self.a2) = (b0, b1, b2, a1, a2);
    }

    /// Filter one sample.
    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Clear the delay line, keeping the coefficients.
    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_lowpass_dc() {
        let mut b = Biquad::new();
        assert_eq!(b.process(3.0), 3.0);
        b.low_pass(1000.0, 48_000);
        let mut y = 0.0;
        for _ in 0..10_000 {
            y = b.process(1.0);
        }
        assert!((y - 1.0).abs() < 1e-6, "low-pass passes DC, got {y}");
        b.high_pass(1000.0, 48_000);
        b.reset();
        for _ in 0..10_000 {
            y = b.process(1.0);
        }
        assert!(y.abs() < 1e-6, "high-pass blocks DC, got {y}");
    }
}
