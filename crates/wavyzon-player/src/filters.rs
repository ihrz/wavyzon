//! Client-facing filter settings (Lavalink shapes plus `reverb` and
//! `pluginFilters`) and the built-in effect chain that renders them.

use std::collections::BTreeMap;
use std::f64::consts::{PI, SQRT_2};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use wavyzon_audio::dsp::Biquad;
use wavyzon_audio::{Frame, SAMPLE_RATE, clip};

use crate::filter_ramp::{WET_COUNT, WET_KARAOKE, WET_LOW_PASS, WET_REVERB, WET_ROTATION, WET_TREMOLO, WET_VIBRATO};
pub use crate::reverb::Reverb;
use crate::reverb::ReverbState;

/// Centre frequencies of the 15 equalizer bands (Lavalink's layout).
pub const EQUALIZER_BANDS_HZ: [f64; 15] = [
    25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0, 4000.0, 6300.0, 10000.0, 16000.0,
];

/// One equalizer band. `gain` is clamped to `-0.25..=1` (`0` = flat,
/// `-0.25` mutes the band, `1` doubles it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Band {
    /// Band index, `0..15`.
    pub band: i32,
    /// Gain multiplier offset.
    pub gain: f64,
}

/// Lavalink `timescale`. Zero or negative values mean `1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timescale {
    /// Tempo without pitch change.
    pub speed: f64,
    /// Pitch without tempo change.
    pub pitch: f64,
    /// Tempo and pitch together (resampling).
    pub rate: f64,
}

/// Lavalink `tremolo` / `vibrato`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Oscillator {
    /// Oscillation frequency, Hz (`0` = off).
    pub frequency: f64,
    /// Depth, `0..=1`.
    pub depth: f64,
}

/// Lavalink `rotation` (8D audio).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rotation {
    /// Rotation frequency, Hz (`0` = off).
    pub rotation_hz: f64,
}

/// Lavalink `lowPass`: a one-pole smoother; values `<= 1` disable it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LowPass {
    /// Smoothing factor.
    pub smoothing: f64,
}

/// Lavalink `karaoke` (centre-channel removal). Only `level` is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Karaoke {
    /// How much of the centre to remove, `0..=1`.
    pub level: f64,
    /// Accepted for Lavalink compatibility.
    pub mono_level: f64,
}

/// The full `filters` object of a player update. Absent fields are off.
///
/// ```json
/// { "equalizer": [{ "band": 0, "gain": 0.2 }],
///   "timescale": { "speed": 1.1, "pitch": 1, "rate": 1 },
///   "reverb": { "mix": 0.3, "room": 0.85, "damping": 0.4 },
///   "pluginFilters": { "bitcrush": { "bits": 6 } } }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Filters {
    /// Up to 15 bands.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub equalizer: Vec<Band>,
    /// Speed / pitch / rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timescale: Option<Timescale>,
    /// Amplitude modulation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tremolo: Option<Oscillator>,
    /// Pitch modulation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vibrato: Option<Oscillator>,
    /// Stereo rotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<Rotation>,
    /// One-pole low-pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_pass: Option<LowPass>,
    /// Centre removal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub karaoke: Option<Karaoke>,
    /// Freeverb (Wavyzon / Cadence extension).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverb: Option<Reverb>,
    /// Plugin filters by registered name, each with its own JSON config.
    /// They run after the built-in chain, in name order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub plugin_filters: BTreeMap<String, Value>,
}

/// Treat JSON `null` like an absent field.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn positive(v: f64, fallback: f64) -> f64 {
    if v <= 0.0 { fallback } else { v }
}

impl Filters {
    /// Resampling factor (`rate * pitch`).
    pub(crate) fn resample_factor(&self) -> f64 {
        self.timescale
            .map_or(1.0, |t| positive(t.rate, 1.0) * positive(t.pitch, 1.0))
    }

    /// Stretcher tempo (`speed / pitch`, pitch being undone by the stretcher).
    pub(crate) fn tempo(&self) -> f64 {
        self.timescale
            .map_or(1.0, |t| positive(t.speed, 1.0) / positive(t.pitch, 1.0))
    }

    /// How fast the source position advances relative to wall time
    /// (`speed * rate`). Used for the `rate` of [`State`](crate::State).
    pub fn position_rate(&self) -> f64 {
        self.timescale
            .map_or(1.0, |t| positive(t.speed, 1.0) * positive(t.rate, 1.0))
    }
}

/// What the ramp says to render this frame: eq gains mid-glide and the
/// (possibly fading) effects with their last settings.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Effective {
    pub eq: [f64; 15],
    pub karaoke: Option<Karaoke>,
    pub low_pass: Option<LowPass>,
    pub vibrato: Option<Oscillator>,
    pub tremolo: Option<Oscillator>,
    pub rotation: Option<Rotation>,
    pub reverb: Option<Reverb>,
}

impl Effective {
    fn active(&self) -> bool {
        self.eq.iter().any(|g| *g != 0.0)
            || self.karaoke.is_some()
            || self.low_pass.is_some()
            || self.vibrato.is_some()
            || self.tremolo.is_some()
            || self.rotation.is_some()
            || self.reverb.is_some()
    }
}

const VIBRATO_MAX_DELAY: usize = 480;

/// State of the built-in effect chain for one deck.
#[derive(Debug, Clone)]
pub(crate) struct Effects {
    eq_l: [Biquad; 15],
    eq_r: [Biquad; 15],
    eq_gains: [f64; 15],
    eq_ready: bool,
    low_l: f64,
    low_r: f64,
    phase: f64,
    rot_phase: f64,
    vib_phase: f64,
    delay_l: [f64; VIBRATO_MAX_DELAY],
    delay_r: [f64; VIBRATO_MAX_DELAY],
    delay_idx: usize,
    rev: ReverbState,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            eq_l: [Biquad::new(); 15],
            eq_r: [Biquad::new(); 15],
            eq_gains: [0.0; 15],
            eq_ready: false,
            low_l: 0.0,
            low_r: 0.0,
            phase: 0.0,
            rot_phase: 0.0,
            vib_phase: 0.0,
            delay_l: [0.0; VIBRATO_MAX_DELAY],
            delay_r: [0.0; VIBRATO_MAX_DELAY],
            delay_idx: 0,
            rev: ReverbState::default(),
        }
    }
}

/// Peaking EQ coefficients (Q = 1.1) for a Lavalink gain at `hz`.
fn peaking(hz: f64, gain: f64) -> (f64, f64, f64, f64, f64) {
    let db = if gain <= -1.0 {
        -40.0
    } else {
        20.0 * (1.0 + gain).log10()
    };
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * PI * hz / SAMPLE_RATE as f64;
    let alpha = w0.sin() / (2.0 * 1.1);
    let cos = w0.cos();
    let a0 = 1.0 + alpha / a;
    (
        (1.0 + alpha * a) / a0,
        (-2.0 * cos) / a0,
        (1.0 - alpha * a) / a0,
        (-2.0 * cos) / a0,
        (1.0 - alpha / a) / a0,
    )
}

#[inline]
fn mix(dry: f64, wet: f64, amount: f64) -> f64 {
    dry + (wet - dry) * amount
}

impl Effects {
    fn configure_eq(&mut self, bands: &[f64; 15]) -> bool {
        let mut gains = [0.0; 15];
        let mut active = false;
        for (g, b) in gains.iter_mut().zip(bands) {
            *g = b.clamp(-0.25, 1.0);
            active |= *g != 0.0;
        }
        if !active {
            return false;
        }
        if self.eq_ready && gains == self.eq_gains {
            return true;
        }
        for (i, hz) in EQUALIZER_BANDS_HZ.iter().enumerate() {
            let (b0, b1, b2, a1, a2) = peaking(*hz, gains[i]);
            self.eq_l[i].coefficients(b0, b1, b2, a1, a2);
            self.eq_r[i].coefficients(b0, b1, b2, a1, a2);
        }
        self.eq_gains = gains;
        self.eq_ready = true;
        true
    }

    /// Render `f` (the ramp's current view) into `out` in place.
    pub(crate) fn apply(&mut self, f: &Effective, wet: &[f64; WET_COUNT], out: &mut Frame) {
        if !f.active() {
            return;
        }
        let eq = self.configure_eq(&f.eq);
        let dt = 1.0 / SAMPLE_RATE as f64;
        for s in out.chunks_exact_mut(2) {
            let (mut l, mut r) = (s[0] as f64, s[1] as f64);
            if let Some(k) = f.karaoke {
                let level = k.level.clamp(0.0, 1.0) * wet[WET_KARAOKE];
                let mid = (l + r) / 2.0;
                l -= mid * level;
                r -= mid * level;
            }
            if eq {
                for b in 0..15 {
                    l = self.eq_l[b].process(l);
                    r = self.eq_r[b].process(r);
                }
            }
            if let Some(lp) = f.low_pass.filter(|lp| lp.smoothing > 1.0) {
                self.low_l += (l - self.low_l) / lp.smoothing;
                self.low_r += (r - self.low_r) / lp.smoothing;
                l = mix(l, self.low_l, wet[WET_LOW_PASS]);
                r = mix(r, self.low_r, wet[WET_LOW_PASS]);
            }
            if let Some(v) = f.vibrato.filter(|v| v.frequency > 0.0) {
                self.delay_l[self.delay_idx] = l;
                self.delay_r[self.delay_idx] = r;
                let depth = v.depth.clamp(0.0, 1.0);
                let lag = ((0.5 + 0.5 * self.vib_phase.sin()) * depth * (VIBRATO_MAX_DELAY - 2) as f64) as usize;
                let j = (self.delay_idx + VIBRATO_MAX_DELAY - lag) % VIBRATO_MAX_DELAY;
                l = mix(l, self.delay_l[j], wet[WET_VIBRATO]);
                r = mix(r, self.delay_r[j], wet[WET_VIBRATO]);
                self.delay_idx = (self.delay_idx + 1) % VIBRATO_MAX_DELAY;
                self.vib_phase += 2.0 * PI * v.frequency * dt;
            }
            if let Some(t) = f.tremolo.filter(|t| t.frequency > 0.0) {
                let depth = t.depth.clamp(0.0, 1.0) * wet[WET_TREMOLO];
                let g = 1.0 - depth * (0.5 + 0.5 * self.phase.sin());
                l *= g;
                r *= g;
                self.phase += 2.0 * PI * t.frequency * dt;
            }
            if let Some(rot) = f.rotation.filter(|r| r.rotation_hz != 0.0) {
                let angle = (self.rot_phase.sin() + 1.0) * PI / 4.0;
                let (rl, rr) = (l * angle.cos() * SQRT_2, r * angle.sin() * SQRT_2);
                l = mix(l, rl, wet[WET_ROTATION]);
                r = mix(r, rr, wet[WET_ROTATION]);
                self.rot_phase += 2.0 * PI * rot.rotation_hz * dt;
            }
            if let Some(rv) = &f.reverb {
                (l, r) = self.rev.process(l, r, rv, wet[WET_REVERB]);
            }
            s[0] = clip(l);
            s[1] = clip(r);
        }
        self.phase %= 2.0 * PI;
        self.rot_phase %= 2.0 * PI;
        self.vib_phase %= 2.0 * PI;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_matches_cadence() {
        let f: Filters = serde_json::from_str(
            r#"{"equalizer":null,"timescale":{"speed":1.5},"lowPass":{"smoothing":20},
                "rotation":{"rotationHz":0.2},"pluginFilters":{"x":{"a":1}},"volume":1.0}"#,
        )
        .unwrap();
        assert!(f.equalizer.is_empty());
        assert_eq!(f.timescale.unwrap().speed, 1.5);
        assert_eq!(f.position_rate(), 1.5);
        assert_eq!(f.plugin_filters["x"]["a"], 1);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["lowPass"]["smoothing"], 20.0);
        assert_eq!(v["rotation"]["rotationHz"], 0.2);
        assert!(v.get("equalizer").is_none() && v.get("reverb").is_none());
        assert_eq!(serde_json::to_string(&Filters::default()).unwrap(), "{}");
    }
}
