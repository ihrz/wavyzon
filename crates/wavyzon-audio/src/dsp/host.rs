//! Gain shapes for "host" transitions: a voice-over (DJ drop, announcement)
//! that plays between two tracks, ducking the incoming one.

use std::f64::consts::FRAC_PI_2;

use crate::{CHANNELS, FRAME_SAMPLES, FRAME_SAMPLES_PER_CHANNEL, Frame, clip};

/// Timing of a host segment, all in ms relative to the voice's start.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostShape {
    /// Length of the voice clip.
    pub voice_ms: i64,
    /// How long the outgoing track takes to fade out under the voice.
    pub out_fade_ms: i64,
    /// How long before the voice ends the incoming track starts (ducked).
    pub lead_ms: i64,
    /// Rise of the incoming track from the duck to full level after the voice.
    pub ramp_ms: i64,
    /// Linear gain of the incoming track under the voice (see [`duck_gain`]).
    pub duck: f64,
}

/// Linear gain for a duck of `db` decibels (the sign is ignored).
pub fn duck_gain(db: f64) -> f64 {
    10f64.powf(-db.abs() / 20.0)
}

/// Smoothstep on `0..=1`.
fn smooth(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

impl HostShape {
    /// Length of the whole segment (voice + ramp).
    pub fn total_ms(&self) -> i64 {
        self.voice_ms + self.ramp_ms
    }

    /// When the incoming track starts playing.
    pub fn in_open_ms(&self) -> i64 {
        (self.voice_ms - self.lead_ms).max(0)
    }

    /// Outgoing gain at `t` (quarter-cosine fade; `0` without a fade).
    pub fn out_gain(&self, t: i64) -> f64 {
        if self.out_fade_ms <= 0 || t >= self.out_fade_ms {
            return 0.0;
        }
        if t <= 0 {
            return 1.0;
        }
        (FRAC_PI_2 * t as f64 / self.out_fade_ms as f64).cos()
    }

    /// Incoming gain at `t`: silent, rising to `duck` under the voice's end,
    /// then rising to `1` over `ramp_ms`.
    pub fn in_gain(&self, t: i64) -> f64 {
        let open = self.in_open_ms();
        if t < open {
            0.0
        } else if t < self.voice_ms {
            self.duck * smooth((t - open) as f64 / (self.voice_ms - open).max(1) as f64)
        } else if t < self.total_ms() {
            self.duck + (1.0 - self.duck) * smooth((t - self.voice_ms) as f64 / self.ramp_ms.max(1) as f64)
        } else {
            1.0
        }
    }

    /// Whether the incoming track plays at `t`.
    pub fn in_open(&self, t: i64) -> bool {
        t >= self.in_open_ms()
    }
}

/// A floating-point summing bus for one frame, so several sources can be
/// added without clipping between them.
#[derive(Debug, Clone, PartialEq)]
pub struct Bus(pub [f64; FRAME_SAMPLES]);

impl Default for Bus {
    fn default() -> Self {
        Bus([0.0; FRAME_SAMPLES])
    }
}

impl Bus {
    /// Zero the bus.
    pub fn reset(&mut self) {
        self.0 = [0.0; FRAME_SAMPLES];
    }

    /// Add `src` with a gain ramping linearly from `from` to `to`.
    pub fn add(&mut self, src: &Frame, from: f64, to: f64) {
        if from == 0.0 && to == 0.0 {
            return;
        }
        for i in 0..FRAME_SAMPLES_PER_CHANNEL {
            let g = from + (to - from) * (i + 1) as f64 / FRAME_SAMPLES_PER_CHANNEL as f64;
            for c in 0..CHANNELS {
                let k = i * CHANNELS + c;
                self.0[k] += src[k] as f64 * g;
            }
        }
    }

    /// Write the sum, saturated, to `out`.
    pub fn write(&self, out: &mut Frame) {
        for (o, &v) in out.iter_mut().zip(self.0.iter()) {
            *o = clip(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_MS, SILENCE};

    fn shape() -> HostShape {
        HostShape {
            voice_ms: 6000,
            out_fade_ms: 2500,
            lead_ms: 1800,
            ramp_ms: 1200,
            duck: duck_gain(9.0),
        }
    }
    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn duck_gain_is_in_decibels() {
        assert!((duck_gain(9.0) - 0.3548).abs() <= 1e-3);
        assert_eq!(duck_gain(-9.0), duck_gain(9.0));
    }

    #[test]
    fn outgoing_fades_out_under_the_start_of_the_voice() {
        let s = shape();
        assert!(near(s.out_gain(0), 1.0));
        let g = s.out_gain(1250);
        assert!(g > 0.0 && g < 1.0, "mid-fade gain = {g}");
        assert!(s.out_gain(2500) == 0.0 && s.out_gain(4000) == 0.0);
        assert_eq!(HostShape::default().out_gain(0), 0.0);
    }

    #[test]
    fn incoming_is_held_until_shortly_before_the_voice_ends() {
        let s = shape();
        assert_eq!(s.in_open_ms(), 4200);
        assert!(!s.in_open(4199) && s.in_gain(4199) == 0.0);
        assert!(s.in_open(4200));
    }

    #[test]
    fn incoming_rises_to_the_duck_under_the_voice_then_to_full() {
        let s = shape();
        let g = s.in_gain(5000);
        assert!(g > 0.0 && g < s.duck);
        assert!(near(s.in_gain(6000), s.duck));
        let g = s.in_gain(6600);
        assert!(g > s.duck && g < 1.0);
        assert!(s.in_gain(7200) == 1.0 && s.total_ms() == 7200);
    }

    #[test]
    fn gains_never_step() {
        let s = shape();
        let (mut prev_in, mut prev_out) = (s.in_gain(0), s.out_gain(0));
        let mut ms = FRAME_MS;
        while ms <= s.total_ms() {
            let (i, o) = (s.in_gain(ms), s.out_gain(ms));
            assert!(
                (i - prev_in).abs() <= 0.06 && (o - prev_out).abs() <= 0.06,
                "gain jumped at {ms}ms"
            );
            prev_in = i;
            prev_out = o;
            ms += FRAME_MS;
        }
    }

    #[test]
    fn a_lead_longer_than_the_voice_opens_at_once() {
        let s = HostShape {
            voice_ms: 1000,
            lead_ms: 5000,
            ramp_ms: 500,
            duck: 0.5,
            ..Default::default()
        };
        assert!(s.in_open_ms() == 0 && s.in_open(0));
    }

    #[test]
    fn bus_sums_without_clipping_between_sources() {
        let (a, b, mut out) = ([30000i16; FRAME_SAMPLES], [-30000i16; FRAME_SAMPLES], SILENCE);
        let mut bus = Bus::default();
        bus.add(&a, 1.0, 1.0);
        bus.add(&b, 1.0, 1.0);
        bus.write(&mut out);
        assert_eq!(out[0], 0);
        bus.reset();
        bus.add(&a, 0.0, 0.0);
        bus.write(&mut out);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn bus_ramps_per_sample() {
        let (src, mut out) = ([10000i16; FRAME_SAMPLES], SILENCE);
        let mut bus = Bus::default();
        bus.add(&src, 0.0, 1.0);
        bus.write(&mut out);
        assert!(out[0] < out[FRAME_SAMPLES - 1] && out[FRAME_SAMPLES - 1] == 10000);
    }
}
