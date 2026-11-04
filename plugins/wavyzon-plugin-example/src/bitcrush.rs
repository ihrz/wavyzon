//! A `bitcrush` audio filter: bit-depth reduction plus sample-and-hold
//! downsampling, the classic lo-fi / chiptune effect.
//!
//! ```json
//! { "filters": { "pluginFilters": { "bitcrush": { "bits": 6, "downsample": 4, "mix": 1.0 } } } }
//! ```

use wavyzon_plugin::serde_json::Value;
use wavyzon_plugin::{AudioFilter, CHANNELS, Failure, FilterFactory};

/// Validated filter settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BitcrushConfig {
    /// Bit depth, `1..=16` (default `8`).
    pub bits: u32,
    /// Keep one sample in `downsample`, `1..=64` (default `1`, off).
    pub downsample: u32,
    /// Dry/wet, `0.0..=1.0` (default `1.0`).
    pub mix: f32,
}

impl Default for BitcrushConfig {
    fn default() -> Self {
        Self {
            bits: 8,
            downsample: 1,
            mix: 1.0,
        }
    }
}

impl BitcrushConfig {
    /// Parse the client's JSON. Missing fields take their default; wrong
    /// types or out-of-range values are errors (sent to the client as 400).
    pub fn parse(config: &Value) -> Result<Self, Failure> {
        let bad = |msg: &str| Failure::common("bitcrush", "invalid bitcrush filter", Some(msg.to_string()));
        let obj = match config {
            Value::Object(o) => o,
            Value::Null => return Ok(Self::default()),
            _ => return Err(bad("expected an object")),
        };
        let mut c = Self::default();
        if let Some(v) = obj.get("bits") {
            let b = v.as_u64().ok_or_else(|| bad("bits must be an integer"))?;
            if !(1..=16).contains(&b) {
                return Err(bad("bits must be between 1 and 16"));
            }
            c.bits = b as u32;
        }
        if let Some(v) = obj.get("downsample") {
            let d = v.as_u64().ok_or_else(|| bad("downsample must be an integer"))?;
            if !(1..=64).contains(&d) {
                return Err(bad("downsample must be between 1 and 64"));
            }
            c.downsample = d as u32;
        }
        if let Some(v) = obj.get("mix") {
            let m = v.as_f64().ok_or_else(|| bad("mix must be a number"))?;
            if !(0.0..=1.0).contains(&m) {
                return Err(bad("mix must be between 0 and 1"));
            }
            c.mix = m as f32;
        }
        Ok(c)
    }
}

/// A running bitcrusher. No allocation and no locking in [`process`](AudioFilter::process).
#[derive(Debug, Clone)]
pub struct Bitcrush {
    cfg: BitcrushConfig,
    /// Quantisation steps for one polarity: `2^(bits-1)`.
    steps: f32,
    held: [f32; CHANNELS],
    /// Frames left before the next sample is taken.
    countdown: u32,
}

impl Bitcrush {
    pub fn new(cfg: BitcrushConfig) -> Self {
        Self {
            cfg,
            steps: (1u32 << (cfg.bits - 1)) as f32,
            held: [0.0; CHANNELS],
            countdown: 0,
        }
    }

    pub fn config(&self) -> BitcrushConfig {
        self.cfg
    }

    fn quantise(&self, v: f32) -> f32 {
        ((v * self.steps).round() / self.steps).clamp(-1.0, 1.0)
    }
}

impl AudioFilter for Bitcrush {
    fn process(&mut self, frame: &mut [f32]) {
        let wet = self.cfg.mix;
        let dry = 1.0 - wet;
        for pair in frame.chunks_exact_mut(CHANNELS) {
            if self.countdown == 0 {
                for (ch, s) in pair.iter().enumerate() {
                    self.held[ch] = self.quantise(*s);
                }
                self.countdown = self.cfg.downsample;
            }
            self.countdown -= 1;
            for (ch, s) in pair.iter_mut().enumerate() {
                *s = *s * dry + self.held[ch] * wet;
            }
        }
    }

    fn reconfigure(&mut self, config: &Value) -> bool {
        match BitcrushConfig::parse(config) {
            Ok(cfg) => {
                self.cfg = cfg;
                self.steps = (1u32 << (cfg.bits - 1)) as f32;
                true
            }
            // Let the node call the factory, which reports the error.
            Err(_) => false,
        }
    }

    fn reset(&mut self) {
        self.held = [0.0; CHANNELS];
        self.countdown = 0;
    }
}

/// Registered with [`Registry::filter`](wavyzon_plugin::Registry::filter).
pub struct BitcrushFactory;

impl FilterFactory for BitcrushFactory {
    fn name(&self) -> &str {
        "bitcrush"
    }

    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        Ok(Box::new(Bitcrush::new(BitcrushConfig::parse(config)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavyzon_plugin::FRAME_SAMPLES;
    use wavyzon_plugin::serde_json::json;

    #[test]
    fn parses_and_validates() {
        assert_eq!(BitcrushConfig::parse(&json!({})).unwrap(), BitcrushConfig::default());
        assert_eq!(BitcrushConfig::parse(&Value::Null).unwrap(), BitcrushConfig::default());
        let c = BitcrushConfig::parse(&json!({"bits": 4, "downsample": 8, "mix": 0.5})).unwrap();
        assert_eq!((c.bits, c.downsample, c.mix), (4, 8, 0.5));
        assert!(BitcrushConfig::parse(&json!({"bits": 0})).is_err());
        assert!(BitcrushConfig::parse(&json!({"bits": 17})).is_err());
        assert!(BitcrushConfig::parse(&json!({"bits": "8"})).is_err());
        assert!(BitcrushConfig::parse(&json!({"downsample": 65})).is_err());
        assert!(BitcrushConfig::parse(&json!({"mix": 1.5})).is_err());
        assert!(BitcrushConfig::parse(&json!([1])).is_err());
        assert!(BitcrushFactory.create(&json!({"bits": 99})).is_err());
    }

    #[test]
    fn quantises_to_bit_depth() {
        // 2 bits → steps of 0.5
        let mut f = Bitcrush::new(BitcrushConfig {
            bits: 2,
            downsample: 1,
            mix: 1.0,
        });
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        frame[0] = 0.3;
        frame[1] = -0.8;
        f.process(&mut frame);
        assert_eq!(frame[0], 0.5);
        assert_eq!(frame[1], -1.0);
        assert!(frame.iter().all(|s| (s * 2.0).fract() == 0.0));
    }

    #[test]
    fn sample_and_hold_downsamples() {
        let mut f = Bitcrush::new(BitcrushConfig {
            bits: 16,
            downsample: 4,
            mix: 1.0,
        });
        let mut frame: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i / 2) as f32 / 2000.0).collect();
        f.process(&mut frame);
        // frames 0..4 all hold frame 0, frames 4..8 hold frame 4
        let left: Vec<f32> = frame.iter().step_by(2).copied().take(8).collect();
        assert!(left[..4].iter().all(|v| *v == left[0]));
        assert!(left[4..].iter().all(|v| *v == left[4]));
        assert_ne!(left[0], left[4]);
    }

    #[test]
    fn dry_mix_is_transparent_and_reconfigure_applies() {
        let mut f = Bitcrush::new(BitcrushConfig {
            bits: 1,
            downsample: 1,
            mix: 0.0,
        });
        let mut frame = vec![0.123f32; FRAME_SAMPLES];
        f.process(&mut frame);
        assert!(frame.iter().all(|s| (*s - 0.123).abs() < 1e-6));

        assert!(f.reconfigure(&json!({"bits": 3, "mix": 1.0})));
        assert_eq!(f.config().bits, 3);
        assert!(!f.reconfigure(&json!({"bits": 0})));
        assert_eq!(f.config().bits, 3, "a rejected config leaves the filter unchanged");
    }
}
