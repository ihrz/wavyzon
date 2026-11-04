//! A Freeverb-style reverb (8 combs and 4 all-passes per channel, tuned for
//! 48 kHz).

use serde::{Deserialize, Serialize};
use wavyzon_audio::SAMPLE_RATE;

/// The `filters.reverb` setting. Every value is `0..=1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Reverb {
    /// Wet amount (`0` = dry, effect off).
    pub mix: f64,
    /// Room size (comb feedback).
    pub room: f64,
    /// High-frequency damping of the tail.
    pub damping: f64,
}

const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const REVERB_SPREAD: usize = 23;
const REVERB_INPUT_GAIN: f64 = 0.015;
const ALLPASS_FEEDBACK: f64 = 0.5;

#[derive(Debug, Clone, Default)]
struct Comb {
    buf: Vec<f64>,
    idx: usize,
    store: f64,
}

impl Comb {
    #[inline]
    fn process(&mut self, input: f64, feedback: f64, damp: f64) -> f64 {
        let out = self.buf[self.idx];
        self.store = out * (1.0 - damp) + self.store * damp;
        self.buf[self.idx] = input + self.store * feedback;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
struct Allpass {
    buf: Vec<f64>,
    idx: usize,
}

impl Allpass {
    #[inline]
    fn process(&mut self, input: f64) -> f64 {
        let held = self.buf[self.idx];
        let out = held - input;
        self.buf[self.idx] = input + held * ALLPASS_FEEDBACK;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

/// Freeverb delay lines, allocated on first use.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReverbState {
    ready: bool,
    comb_l: [Comb; 8],
    comb_r: [Comb; 8],
    ap_l: [Allpass; 4],
    ap_r: [Allpass; 4],
}

/// Scale a 44.1 kHz tuning to 48 kHz.
fn scaled(samples: usize) -> usize {
    ((samples as f64 * SAMPLE_RATE as f64 / 44_100.0).round() as usize).max(1)
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

impl ReverbState {
    fn init(&mut self) {
        for (i, n) in COMB_TUNING.iter().enumerate() {
            self.comb_l[i].buf = vec![0.0; scaled(*n)];
            self.comb_r[i].buf = vec![0.0; scaled(n + REVERB_SPREAD)];
        }
        for (i, n) in ALLPASS_TUNING.iter().enumerate() {
            self.ap_l[i].buf = vec![0.0; scaled(*n)];
            self.ap_r[i].buf = vec![0.0; scaled(n + REVERB_SPREAD)];
        }
        self.ready = true;
    }

    /// Process one stereo sample; `wet` is the ramp's fade factor (`0..=1`).
    pub(crate) fn process(&mut self, l: f64, r: f64, rv: &Reverb, wet: f64) -> (f64, f64) {
        if !self.ready {
            self.init();
        }
        let feedback = clamp01(rv.room) * 0.28 + 0.7;
        let damp = clamp01(rv.damping) * 0.4;
        let input = (l + r) * REVERB_INPUT_GAIN;
        let (mut out_l, mut out_r) = (0.0, 0.0);
        for i in 0..self.comb_l.len() {
            out_l += self.comb_l[i].process(input, feedback, damp);
            out_r += self.comb_r[i].process(input, feedback, damp);
        }
        for i in 0..self.ap_l.len() {
            out_l = self.ap_l[i].process(out_l);
            out_r = self.ap_r[i].process(out_r);
        }
        let amount = clamp01(rv.mix) * wet;
        (
            l * (1.0 - amount * 0.5) + out_l * amount * 3.0,
            r * (1.0 - amount * 0.5) + out_r * amount * 3.0,
        )
    }
}
