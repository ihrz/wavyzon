//! Smooths filter changes: eq gains, pitch and speed glide toward their
//! targets (a one-pole ramp, ~150 ms time constant), and effects that turn on
//! or off fade their wet amount while keeping their last settings.

use wavyzon_audio::FRAME_MS;

use crate::filters::{Band, Effective, Filters};

/// Time constant of every filter glide, in ms.
pub const FILTER_RAMP_TAU_MS: f64 = 150.0;
const RAMP_SNAP: f64 = 1e-3;

pub(crate) const WET_KARAOKE: usize = 0;
pub(crate) const WET_LOW_PASS: usize = 1;
pub(crate) const WET_VIBRATO: usize = 2;
pub(crate) const WET_TREMOLO: usize = 3;
pub(crate) const WET_ROTATION: usize = 4;
pub(crate) const WET_REVERB: usize = 5;
pub(crate) const WET_COUNT: usize = 6;

/// Per-frame glide coefficient.
pub(crate) fn ramp_alpha() -> f64 {
    1.0 - (-(FRAME_MS as f64) / FILTER_RAMP_TAU_MS).exp()
}

/// One glide step from `cur` toward `target`, snapping when close.
pub(crate) fn glide(cur: f64, target: f64) -> f64 {
    let next = cur + (target - cur) * ramp_alpha();
    if (target - next).abs() < RAMP_SNAP {
        target
    } else {
        next
    }
}

fn eq_targets(bands: &[Band]) -> [f64; 15] {
    let mut gains = [0.0; 15];
    for b in bands {
        if (0..15).contains(&b.band) {
            gains[b.band as usize] = b.gain.clamp(-0.25, 1.0);
        }
    }
    gains
}

fn wet_targets(f: &Filters) -> [f64; WET_COUNT] {
    let on = |b: bool| if b { 1.0 } else { 0.0 };
    let mut w = [0.0; WET_COUNT];
    w[WET_KARAOKE] = on(f.karaoke.is_some());
    w[WET_LOW_PASS] = on(f.low_pass.is_some_and(|l| l.smoothing > 1.0));
    w[WET_VIBRATO] = on(f.vibrato.is_some_and(|v| v.frequency > 0.0));
    w[WET_TREMOLO] = on(f.tremolo.is_some_and(|t| t.frequency > 0.0));
    w[WET_ROTATION] = on(f.rotation.is_some_and(|r| r.rotation_hz != 0.0));
    w[WET_REVERB] = on(f.reverb.is_some_and(|r| r.mix > 0.0));
    w
}

/// Glide state of one deck.
#[derive(Debug, Clone, Default)]
pub(crate) struct FilterRamp {
    pub ready: bool,
    pub eq: [f64; 15],
    pub resample: f64,
    pub tempo: f64,
    pub wet: [f64; WET_COUNT],
    held: Effective,
}

fn pick<T>(w: f64, v: Option<T>) -> Option<T> {
    if w > 0.0 { v } else { None }
}

impl FilterRamp {
    fn hold(&mut self, t: &Filters) {
        if t.karaoke.is_some() {
            self.held.karaoke = t.karaoke;
        }
        if t.low_pass.is_some() {
            self.held.low_pass = t.low_pass;
        }
        if t.vibrato.is_some() {
            self.held.vibrato = t.vibrato;
        }
        if t.tremolo.is_some() {
            self.held.tremolo = t.tremolo;
        }
        if t.rotation.is_some() {
            self.held.rotation = t.rotation;
        }
        if t.reverb.is_some() {
            self.held.reverb = t.reverb;
        }
    }

    /// Advance one frame toward `t`. The first call jumps straight to it.
    pub(crate) fn advance(&mut self, t: &Filters) {
        let (eq, wet) = (eq_targets(&t.equalizer), wet_targets(t));
        self.hold(t);
        if !self.ready {
            self.eq = eq;
            self.wet = wet;
            self.resample = t.resample_factor();
            self.tempo = t.tempo();
            self.ready = true;
            return;
        }
        for (c, t) in self.eq.iter_mut().zip(eq) {
            *c = glide(*c, t);
        }
        for (c, t) in self.wet.iter_mut().zip(wet) {
            *c = glide(*c, t);
        }
        self.resample = glide(self.resample, t.resample_factor());
        self.tempo = glide(self.tempo, t.tempo());
    }

    /// What to render this frame.
    pub(crate) fn effective(&self) -> Effective {
        Effective {
            eq: self.eq,
            karaoke: pick(self.wet[WET_KARAOKE], self.held.karaoke),
            low_pass: pick(self.wet[WET_LOW_PASS], self.held.low_pass),
            vibrato: pick(self.wet[WET_VIBRATO], self.held.vibrato),
            tremolo: pick(self.wet[WET_TREMOLO], self.held.tremolo),
            rotation: pick(self.wet[WET_ROTATION], self.held.rotation),
            reverb: pick(self.wet[WET_REVERB], self.held.reverb),
        }
    }

    /// Current resampling factor (the target's before the first frame).
    pub(crate) fn resample_factor(&self, fallback: &Filters) -> f64 {
        if self.ready {
            self.resample
        } else {
            fallback.resample_factor()
        }
    }

    /// Current stretcher tempo (the target's before the first frame).
    pub(crate) fn tempo_factor(&self, fallback: &Filters) -> f64 {
        if self.ready { self.tempo } else { fallback.tempo() }
    }
}
