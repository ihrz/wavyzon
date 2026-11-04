//! Transition profiles: bass swap and high-pass sweeps during a crossfade.

use serde::{Deserialize, Serialize};

use crate::dsp::MIN_SWEEP_HZ;

const DEFAULT_BASS_SWAP_RAMP: f64 = 0.12;

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// How a crossfade treats each deck, as a function of the fade's progress
/// (`0..=1`). Serialized in camelCase as in Cadence's API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransitionProfile {
    /// Progress at which the bass of the outgoing deck is swapped for the
    /// incoming one's. `None` disables the swap (both basses play).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bass_swap_at: Option<f64>,
    /// Width of the swap, in progress units (default `0.12`).
    #[serde(skip_serializing_if = "is_zero")]
    pub bass_swap_ramp: f64,
    /// High-pass on the outgoing deck at the start of the fade, in Hz.
    #[serde(skip_serializing_if = "is_zero")]
    pub sweep_out_from_hz: f64,
    /// High-pass on the outgoing deck at the end of the fade.
    #[serde(skip_serializing_if = "is_zero")]
    pub sweep_out_to_hz: f64,
    /// High-pass on the incoming deck at the start of the fade.
    #[serde(skip_serializing_if = "is_zero")]
    pub sweep_in_from_hz: f64,
    /// High-pass on the incoming deck at the end of the fade.
    #[serde(skip_serializing_if = "is_zero")]
    pub sweep_in_to_hz: f64,
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

impl TransitionProfile {
    fn ramp(&self) -> f64 {
        if self.bass_swap_ramp <= 0.0 {
            DEFAULT_BASS_SWAP_RAMP
        } else {
            self.bass_swap_ramp
        }
    }

    /// Whether a bass swap is configured.
    pub fn swaps_bass(&self) -> bool {
        self.bass_swap_at.is_some()
    }

    /// Whether the outgoing deck is swept.
    pub fn sweeps_out(&self) -> bool {
        self.sweep_out_to_hz > MIN_SWEEP_HZ || self.sweep_out_from_hz > MIN_SWEEP_HZ
    }

    /// Whether the incoming deck is swept.
    pub fn sweeps_in(&self) -> bool {
        self.sweep_in_to_hz > MIN_SWEEP_HZ || self.sweep_in_from_hz > MIN_SWEEP_HZ
    }

    /// Bass gain of the outgoing deck at `progress`.
    pub fn outgoing_bass_gain(&self, progress: f64) -> f64 {
        match self.bass_swap_at {
            Some(at) if progress > at => clamp01(1.0 - (progress - at) / self.ramp()),
            _ => 1.0,
        }
    }

    /// Bass gain of the incoming deck at `progress`.
    pub fn incoming_bass_gain(&self, progress: f64) -> f64 {
        match self.bass_swap_at {
            None => 1.0,
            Some(at) if progress <= at => 0.0,
            Some(at) => clamp01((progress - at) / self.ramp()),
        }
    }

    /// High-pass cutoff of the outgoing deck at `progress` (`0` = off).
    pub fn outgoing_sweep_hz(&self, progress: f64) -> f64 {
        if !self.sweeps_out() {
            return 0.0;
        }
        self.sweep_out_from_hz + (self.sweep_out_to_hz - self.sweep_out_from_hz) * progress
    }

    /// High-pass cutoff of the incoming deck at `progress` (`0` = off).
    pub fn incoming_sweep_hz(&self, progress: f64) -> f64 {
        if !self.sweeps_in() {
            return 0.0;
        }
        self.sweep_in_from_hz + (self.sweep_in_to_hz - self.sweep_in_from_hz) * progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bass_swap() {
        let p = TransitionProfile {
            bass_swap_at: Some(0.5),
            bass_swap_ramp: 0.1,
            ..Default::default()
        };
        assert!(
            p.outgoing_bass_gain(0.4) == 1.0 && p.incoming_bass_gain(0.4) == 0.0,
            "before swap"
        );
        assert!(
            p.outgoing_bass_gain(0.7) == 0.0 && p.incoming_bass_gain(0.7) == 1.0,
            "after swap"
        );
        let g = p.outgoing_bass_gain(0.55);
        assert!((g - 0.5).abs() < 1e-9, "mid ramp {g}");
        assert!(!TransitionProfile::default().swaps_bass());
    }

    #[test]
    fn json_shape() {
        let p: TransitionProfile = serde_json::from_str(r#"{"bassSwapAt":0.5,"sweepOutToHz":800}"#).unwrap();
        assert_eq!(p.bass_swap_at, Some(0.5));
        assert!(p.sweeps_out() && !p.sweeps_in());
        assert_eq!(serde_json::to_string(&TransitionProfile::default()).unwrap(), "{}");
    }
}
