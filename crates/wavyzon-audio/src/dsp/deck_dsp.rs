//! Per-deck transition DSP: bass kill (for bass swaps) and a high-pass sweep.

use crate::dsp::Biquad;
use crate::{Frame, SAMPLE_RATE, clip};

/// Crossover of the bass band removed during a bass swap.
pub const BASS_HZ: f64 = 220.0;
/// Sweeps at or below this cutoff are treated as "off".
pub const MIN_SWEEP_HZ: f64 = 25.0;

/// Filters for one deck during a transition.
#[derive(Debug, Clone)]
pub struct DeckDsp {
    bass_left: Biquad,
    bass_right: Biquad,
    sweep_left: Biquad,
    sweep_right: Biquad,
    bass_ready: bool,
    sweep_hz: f64,
}

impl Default for DeckDsp {
    fn default() -> Self {
        Self::new()
    }
}

impl DeckDsp {
    /// Fresh filters with nothing applied.
    pub fn new() -> Self {
        Self {
            bass_left: Biquad::new(),
            bass_right: Biquad::new(),
            sweep_left: Biquad::new(),
            sweep_right: Biquad::new(),
            bass_ready: false,
            sweep_hz: -1.0,
        }
    }

    /// Forget all filter state.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Scale the band below [`BASS_HZ`] by `gain` (`1` = untouched). At full
    /// gain the low-pass still runs, so it is primed when the cut starts.
    pub fn apply_bass_gain(&mut self, f: &mut Frame, gain: f64) {
        if gain >= 0.999 {
            self.prime_bass(f);
            return;
        }
        if !self.bass_ready {
            self.bass_left.low_pass(BASS_HZ, SAMPLE_RATE);
            self.bass_right.low_pass(BASS_HZ, SAMPLE_RATE);
            self.bass_ready = true;
        }
        let cut = 1.0 - gain;
        for s in f.chunks_exact_mut(2) {
            let (l, r) = (s[0] as f64, s[1] as f64);
            s[0] = clip(l - cut * self.bass_left.process(l));
            s[1] = clip(r - cut * self.bass_right.process(r));
        }
    }

    /// High-pass at `cutoff_hz`; does nothing at or below [`MIN_SWEEP_HZ`].
    pub fn apply_high_pass(&mut self, f: &mut Frame, cutoff_hz: f64) {
        if cutoff_hz <= MIN_SWEEP_HZ {
            return;
        }
        if (cutoff_hz - self.sweep_hz).abs() > 1.0 {
            self.sweep_left.high_pass(cutoff_hz, SAMPLE_RATE);
            self.sweep_right.high_pass(cutoff_hz, SAMPLE_RATE);
            self.sweep_hz = cutoff_hz;
        }
        for s in f.chunks_exact_mut(2) {
            s[0] = clip(self.sweep_left.process(s[0] as f64));
            s[1] = clip(self.sweep_right.process(s[1] as f64));
        }
    }

    fn prime_bass(&mut self, f: &Frame) {
        if !self.bass_ready {
            return;
        }
        for s in f.chunks_exact(2) {
            self.bass_left.process(s[0] as f64);
            self.bass_right.process(s[1] as f64);
        }
    }
}
