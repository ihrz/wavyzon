//! Crossfade gain curves.

use std::f64::consts::FRAC_PI_2;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::FRAME_MS;

/// Shape of a crossfade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FadeCurve {
    /// Gains move linearly (a dip of about 3 dB in the middle).
    Linear,
    /// `cos`/`sin` gains: constant power across the fade (the default).
    #[default]
    EqualPower,
}

/// An unknown curve name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown curve {0:?}")]
pub struct UnknownCurve(pub String);

impl FromStr for FadeCurve {
    type Err = UnknownCurve;

    /// Accepts `""`/`equal_power`/`equal-power` and `linear`, any case.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.replace('-', "_").to_ascii_uppercase().as_str() {
            "" | "EQUAL_POWER" => Ok(FadeCurve::EqualPower),
            "LINEAR" => Ok(FadeCurve::Linear),
            _ => Err(UnknownCurve(raw.to_string())),
        }
    }
}

impl fmt::Display for FadeCurve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FadeCurve::Linear => "LINEAR",
            FadeCurve::EqualPower => "EQUAL_POWER",
        })
    }
}

impl Serialize for FadeCurve {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FadeCurve {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Parse a curve name (see [`FadeCurve::from_str`]).
pub fn parse_curve(raw: &str) -> Result<FadeCurve, UnknownCurve> {
    raw.parse()
}

/// A running crossfade, advanced one frame at a time.
///
/// `out_gain` applies to the outgoing deck and `in_gain` to the incoming one;
/// before the first [`advance`](Self::advance) they are `1` and `0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Crossfade {
    curve: FadeCurve,
    total: i64,
    frame: i64,
    /// Gain for the outgoing deck at the current frame.
    pub out_gain: f64,
    /// Gain for the incoming deck at the current frame.
    pub in_gain: f64,
}

impl Crossfade {
    /// A fade over `duration_ms` (at least one frame).
    pub fn new(duration_ms: i64, curve: FadeCurve) -> Self {
        Self {
            curve,
            total: (duration_ms / FRAME_MS).max(1),
            frame: 0,
            out_gain: 1.0,
            in_gain: 0.0,
        }
    }

    /// Whether every frame of the fade has been advanced through.
    pub fn finished(&self) -> bool {
        self.frame >= self.total
    }

    /// `0..=1` progress.
    pub fn progress(&self) -> f64 {
        (self.frame as f64 / self.total as f64).clamp(0.0, 1.0)
    }

    /// Length, rounded down to whole frames.
    pub fn duration_ms(&self) -> i64 {
        self.total * FRAME_MS
    }

    /// The curve in use.
    pub fn curve(&self) -> FadeCurve {
        self.curve
    }

    /// Move one frame forward and update the gains.
    pub fn advance(&mut self) {
        if self.finished() {
            self.out_gain = 0.0;
            self.in_gain = 1.0;
            return;
        }
        self.frame += 1;
        let p = self.frame as f64 / self.total as f64;
        match self.curve {
            FadeCurve::Linear => {
                self.out_gain = 1.0 - p;
                self.in_gain = p;
            }
            FadeCurve::EqualPower => {
                self.out_gain = (p * FRAC_PI_2).cos();
                self.in_gain = (p * FRAC_PI_2).sin();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_power_ends_fully_in() {
        let mut c = Crossfade::new(100, FadeCurve::EqualPower);
        while !c.finished() {
            c.advance();
        }
        assert!(
            (c.in_gain - 1.0).abs() < 1e-9 && c.out_gain.abs() < 1e-9,
            "out={} in={}",
            c.out_gain,
            c.in_gain
        );
        assert_eq!(c.duration_ms(), 100);
    }

    #[test]
    fn equal_power_keeps_power() {
        let mut c = Crossfade::new(400, FadeCurve::EqualPower);
        while !c.finished() {
            c.advance();
            let p = c.out_gain * c.out_gain + c.in_gain * c.in_gain;
            assert!((p - 1.0).abs() < 1e-9, "power {p}");
        }
    }

    #[test]
    fn parse() {
        assert_eq!(parse_curve("linear").unwrap(), FadeCurve::Linear);
        assert_eq!(parse_curve("").unwrap(), FadeCurve::EqualPower);
        assert_eq!(parse_curve("equal-power").unwrap(), FadeCurve::EqualPower);
        assert!(parse_curve("bounce").is_err());
        let c: FadeCurve = serde_json::from_str("\"LINEAR\"").unwrap();
        assert_eq!(c, FadeCurve::Linear);
    }
}
