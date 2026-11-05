//! `Reverb:` (Wavyzon extension): a Freeverb-style reverb, 8 combs and 4
//! all-passes per channel, tuned for 48 kHz. Same algorithm and output
//! levels as the player's built-in `filters.reverb`, so a preset sounds like
//! the equivalent `filters.reverb` setting. Kept in the plugin because the
//! player's is private to it and plugins only depend on the shared crates.

use crate::parse::ReverbSpec;

const SAMPLE_RATE: f64 = 48_000.0;
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;
const INPUT_GAIN: f64 = 0.015;
const ALLPASS_FEEDBACK: f64 = 0.5;

struct Comb {
    buf: Vec<f64>,
    idx: usize,
    store: f64,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            idx: 0,
            store: 0.0,
        }
    }

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

struct Allpass {
    buf: Vec<f64>,
    idx: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            idx: 0,
        }
    }

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

/// Scale a 44.1 kHz tuning to 48 kHz.
fn scaled(samples: usize) -> usize {
    ((samples as f64 * SAMPLE_RATE / 44_100.0).round() as usize).max(1)
}

/// A running reverb. Delay lines are allocated in [`new`](Self::new).
pub struct Freeverb {
    spec: ReverbSpec,
    comb_l: Vec<Comb>,
    comb_r: Vec<Comb>,
    ap_l: Vec<Allpass>,
    ap_r: Vec<Allpass>,
}

impl Freeverb {
    pub fn new(spec: ReverbSpec) -> Self {
        Self {
            spec,
            comb_l: COMB_TUNING.iter().map(|n| Comb::new(scaled(*n))).collect(),
            comb_r: COMB_TUNING.iter().map(|n| Comb::new(scaled(n + SPREAD))).collect(),
            ap_l: ALLPASS_TUNING.iter().map(|n| Allpass::new(scaled(*n))).collect(),
            ap_r: ALLPASS_TUNING
                .iter()
                .map(|n| Allpass::new(scaled(n + SPREAD)))
                .collect(),
        }
    }

    /// Process one block per channel in place.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let feedback = self.spec.room.clamp(0.0, 1.0) * 0.28 + 0.7;
        let damp = self.spec.damping.clamp(0.0, 1.0) * 0.4;
        let amount = self.spec.mix.clamp(0.0, 1.0);
        if amount == 0.0 {
            return;
        }
        for (sl, sr) in l.iter_mut().zip(r.iter_mut()) {
            let (dl, dr) = (*sl as f64, *sr as f64);
            let input = (dl + dr) * INPUT_GAIN;
            let (mut out_l, mut out_r) = (0.0, 0.0);
            for (cl, cr) in self.comb_l.iter_mut().zip(self.comb_r.iter_mut()) {
                out_l += cl.process(input, feedback, damp);
                out_r += cr.process(input, feedback, damp);
            }
            for (al, ar) in self.ap_l.iter_mut().zip(self.ap_r.iter_mut()) {
                out_l = al.process(out_l);
                out_r = ar.process(out_r);
            }
            *sl = (dl * (1.0 - amount * 0.5) + out_l * amount * 3.0) as f32;
            *sr = (dr * (1.0 - amount * 0.5) + out_r * amount * 3.0) as f32;
        }
    }

    pub fn reset(&mut self) {
        for c in self.comb_l.iter_mut().chain(self.comb_r.iter_mut()) {
            c.buf.fill(0.0);
            c.idx = 0;
            c.store = 0.0;
        }
        for a in self.ap_l.iter_mut().chain(self.ap_r.iter_mut()) {
            a.buf.fill(0.0);
            a.idx = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_a_tail_after_an_impulse_and_resets() {
        let mut rv = Freeverb::new(ReverbSpec {
            mix: 0.5,
            room: 0.8,
            damping: 0.3,
        });
        let (mut l, mut r) = (vec![0.0f32; 960], vec![0.0f32; 960]);
        l[0] = 1.0;
        r[0] = 1.0;
        rv.process(&mut l, &mut r);
        let (mut l2, mut r2) = (vec![0.0f32; 960], vec![0.0f32; 960]);
        rv.process(&mut l2, &mut r2);
        let tail: f32 = l2.iter().chain(&r2).map(|v| v.abs()).sum();
        assert!(tail > 0.01, "a reverb keeps ringing after the input stops");
        assert_ne!(l2, r2, "left and right tails differ (stereo spread)");
        rv.reset();
        let (mut l3, mut r3) = (vec![0.0f32; 960], vec![0.0f32; 960]);
        rv.process(&mut l3, &mut r3);
        assert!(l3.iter().chain(&r3).all(|v| *v == 0.0));
    }

    #[test]
    fn zero_mix_is_transparent() {
        let mut rv = Freeverb::new(ReverbSpec {
            mix: 0.0,
            ..ReverbSpec::default()
        });
        let (mut l, mut r) = (vec![0.25f32; 960], vec![-0.5f32; 960]);
        rv.process(&mut l, &mut r);
        assert!(l.iter().all(|v| *v == 0.25) && r.iter().all(|v| *v == -0.5));
    }
}
