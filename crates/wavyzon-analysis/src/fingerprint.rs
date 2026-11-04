//! The fingerprint JSON format (shared with Cadence and greed's automix) and
//! the in-memory registry of analysed tracks.
//!
//! Field names, order and `null`-vs-`[]` behaviour match Cadence's Go
//! encoding: the rhythm arrays (`beatsMs`, `downbeatsMs`, `phrasesMs`) are
//! always arrays, while `tempoMap`, `sections`, `energy`, `onsets` and
//! `vocals` are `null` when empty. Integral floats are written without a
//! fractional part (`0`, `1`, `128`) as Go does.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Version of the fingerprint schema and algorithms.
pub const ANALYSIS_VERSION: i64 = 3;
/// Identifies the analyser implementation. Kept equal to Cadence's so greed
/// treats Wavyzon fingerprints as interchangeable.
pub const ANALYZER_VERSION: &str = "cadence/1";
/// Fingerprints kept by a [`Registry`] before the least recently stored one
/// is evicted.
pub const REGISTRY_SIZE: usize = 512;

/// One tempo change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TempoPoint {
    /// Where the tempo starts.
    pub time_ms: i64,
    /// Tempo from `time_ms` on.
    #[serde(serialize_with = "gofloat::one")]
    pub bpm: f64,
}

/// Beat grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Rhythm {
    /// Tempo in beats per minute (`0` when none was found).
    #[serde(serialize_with = "gofloat::one")]
    pub bpm: f64,
    /// Tempo confidence, `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub bpm_confidence: f64,
    /// Beats per bar (3 or 4).
    pub meter: i64,
    /// Every beat.
    #[serde(default, deserialize_with = "nullable::de")]
    pub beats_ms: Vec<i64>,
    /// The first beat of every bar.
    #[serde(default, deserialize_with = "nullable::de")]
    pub downbeats_ms: Vec<i64>,
    /// Tempo changes (a single point today).
    #[serde(default, serialize_with = "nullable::ser", deserialize_with = "nullable::de")]
    pub tempo_map: Vec<TempoPoint>,
    /// The first downbeat of every phrase.
    #[serde(default, deserialize_with = "nullable::de")]
    pub phrases_ms: Vec<i64>,
    /// Bars per phrase (8 or 16).
    pub phrase_bars: i64,
}

/// Musical key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Harmony {
    /// Camelot wheel code (`8A`, `11B`…).
    pub camelot: String,
    /// Tonic (`C`, `F#`…).
    pub key: String,
    /// `major` or `minor`.
    pub scale: String,
    /// `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub confidence: f64,
}

/// A structural section of the track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    /// `intro`, `outro`, `build`, `chorus`, `verse`, `breakdown`, `drop` or
    /// `bridge`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Section start.
    pub start_ms: i64,
    /// Section end.
    pub end_ms: i64,
    /// Relative energy, `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub energy: f64,
    /// Index of the beat at `start_ms`.
    pub beat_start: i64,
    /// Index of the beat at `end_ms`.
    pub beat_end: i64,
}

/// Where a DJ-style transition can mix in and out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MixRegions {
    /// First audible point.
    pub intro_start_ms: i64,
    /// Where the track reaches full energy (or vocals start).
    pub intro_end_ms: i64,
    /// Where the track starts winding down.
    pub outro_start_ms: i64,
    /// Last audible point.
    pub outro_end_ms: i64,
}

/// EBU R128 measurement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoudnessResult {
    /// Gated integrated loudness in LUFS (floored at
    /// [`FLOOR_LUFS`](crate::FLOOR_LUFS)).
    #[serde(serialize_with = "gofloat::one")]
    pub integrated_lufs: f64,
    /// Sample peak, `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub peak_amplitude: f64,
}

/// One point of the energy curve (every half second).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnergyPoint {
    /// Point time.
    pub time_ms: i64,
    /// Relative RMS, `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub value: f64,
}

/// A detected onset.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Onset {
    /// Onset time.
    pub time_ms: i64,
    /// Relative spectral flux, `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub strength: f64,
}

/// A span that probably contains singing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VocalActivity {
    /// Span start.
    pub start_ms: i64,
    /// Span end.
    pub end_ms: i64,
    /// `0..=1`.
    #[serde(serialize_with = "gofloat::one")]
    pub confidence: f64,
}

/// Everything known about a track, as served by
/// `GET /v1/analysis/{source}/{identifier}` and consumed by greed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    /// Track source.
    pub source: String,
    /// Track identifier within `source`.
    pub identifier: String,
    /// Analysed duration.
    pub duration_ms: i64,
    /// Beat grid.
    pub rhythm: Rhythm,
    /// Key.
    pub harmony: Harmony,
    /// Structure.
    #[serde(default, serialize_with = "nullable::ser", deserialize_with = "nullable::de")]
    pub sections: Vec<Section>,
    /// Energy curve.
    #[serde(default, serialize_with = "nullable::ser", deserialize_with = "nullable::de")]
    pub energy: Vec<EnergyPoint>,
    /// Onsets.
    #[serde(default, serialize_with = "nullable::ser", deserialize_with = "nullable::de")]
    pub onsets: Vec<Onset>,
    /// Vocal spans.
    #[serde(default, serialize_with = "nullable::ser", deserialize_with = "nullable::de")]
    pub vocals: Vec<VocalActivity>,
    /// Mix-in / mix-out regions.
    pub mix_regions: MixRegions,
    /// Loudness.
    pub loudness: LoudnessResult,
    /// 16-dimensional similarity vector: 12 chroma shares, tempo/200, mean
    /// RMS, mean centroid/8 kHz, mean vocal ratio.
    #[serde(default, serialize_with = "gofloat::many", deserialize_with = "nullable::de")]
    pub vector: Vec<f64>,
    /// [`ANALYSIS_VERSION`].
    pub analysis_version: i64,
    /// [`ANALYZER_VERSION`].
    pub analyzer_version: String,
}

impl Fingerprint {
    /// `source:identifier`, the registry key.
    pub fn key(&self) -> String {
        key(&self.source, &self.identifier)
    }
}

pub(crate) fn key(source: &str, identifier: &str) -> String {
    format!("{source}:{identifier}")
}

/// Go-compatible float encoding: integral values are written as integers.
mod gofloat {
    use serde::Serializer;
    use serde::ser::SerializeSeq;

    struct F(f64);

    impl serde::Serialize for F {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let v = self.0;
            if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
                s.serialize_i64(v as i64)
            } else {
                s.serialize_f64(v)
            }
        }
    }

    pub(super) fn one<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&F(*v), s)
    }

    pub(super) fn many<S: Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for x in v {
            seq.serialize_element(&F(*x))?;
        }
        seq.end()
    }
}

/// Go nil-slice behaviour: an empty vector is written as `null`, and `null`
/// reads back as empty.
mod nullable {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn ser<T: Serialize, S: Serializer>(v: &[T], s: S) -> Result<S::Ok, S::Error> {
        if v.is_empty() {
            s.serialize_none()
        } else {
            v.serialize(s)
        }
    }

    pub(super) fn de<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Vec<T>, D::Error> {
        Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
    }
}

/// Bounded LRU of fingerprints keyed by `source:identifier`.
#[derive(Debug, Default)]
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    /// Store order, oldest first; each key appears once.
    order: VecDeque<String>,
    items: HashMap<String, Arc<Fingerprint>>,
}

impl Registry {
    /// An empty registry holding up to [`REGISTRY_SIZE`] fingerprints.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `f`, marking it most recently used and evicting the
    /// oldest entries beyond [`REGISTRY_SIZE`].
    pub fn store(&self, f: Arc<Fingerprint>) {
        let k = f.key();
        let mut r = self.inner.lock();
        if r.items.insert(k.clone(), f).is_some() {
            if let Some(pos) = r.order.iter().position(|o| *o == k) {
                r.order.remove(pos);
            }
        }
        r.order.push_back(k);
        while r.order.len() > REGISTRY_SIZE {
            if let Some(old) = r.order.pop_front() {
                r.items.remove(&old);
            }
        }
    }

    /// The fingerprint of `source:identifier`, if analysed.
    pub fn get(&self, source: &str, identifier: &str) -> Option<Arc<Fingerprint>> {
        self.inner.lock().items.get(&key(source, identifier)).cloned()
    }

    /// Whether `source:identifier` has been analysed.
    pub fn has(&self, source: &str, identifier: &str) -> bool {
        self.inner.lock().items.contains_key(&key(source, identifier))
    }

    /// The stored loudness of `source:identifier`, for loudness
    /// normalisation.
    pub fn loudness(&self, source: &str, identifier: &str) -> Option<LoudnessResult> {
        self.get(source, identifier).map(|f| f.loudness)
    }

    /// Number of stored fingerprints.
    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_bounded() {
        let r = Registry::new();
        for i in 0..REGISTRY_SIZE + 10 {
            r.store(Arc::new(Fingerprint {
                source: "s".into(),
                identifier: format!("t{i}"),
                ..Default::default()
            }));
        }
        assert_eq!(r.len(), REGISTRY_SIZE);
        assert!(!r.has("s", "t0"));
        assert!(r.has("s", &format!("t{}", REGISTRY_SIZE + 9)));
    }

    #[test]
    fn registry_restore_refreshes_order() {
        let r = Registry::new();
        let fp = |id: &str| {
            Arc::new(Fingerprint {
                source: "s".into(),
                identifier: id.into(),
                ..Default::default()
            })
        };
        r.store(fp("first"));
        for i in 0..REGISTRY_SIZE - 1 {
            r.store(fp(&format!("t{i}")));
        }
        r.store(fp("first"));
        r.store(fp("one-more"));
        assert!(r.has("s", "first"));
        assert!(!r.has("s", "t0"));
    }

    #[test]
    fn json_shape_matches_cadence() {
        let fp = Fingerprint {
            source: "s".into(),
            identifier: "i".into(),
            duration_ms: 1000,
            rhythm: Rhythm {
                bpm: 128.0,
                bpm_confidence: 0.5,
                meter: 4,
                phrase_bars: 16,
                ..Default::default()
            },
            harmony: Harmony {
                camelot: "8A".into(),
                key: "A".into(),
                scale: "minor".into(),
                confidence: 0.0,
            },
            sections: vec![Section {
                kind: "intro".into(),
                start_ms: 0,
                end_ms: 1000,
                energy: 1.0,
                beat_start: 0,
                beat_end: 0,
            }],
            loudness: LoudnessResult {
                integrated_lufs: -70.0,
                peak_amplitude: 0.25,
            },
            vector: vec![0.0, 0.5],
            analysis_version: ANALYSIS_VERSION,
            analyzer_version: ANALYZER_VERSION.into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&fp).unwrap();
        assert_eq!(
            json,
            r#"{"source":"s","identifier":"i","durationMs":1000,"rhythm":{"bpm":128,"bpmConfidence":0.5,"meter":4,"beatsMs":[],"downbeatsMs":[],"tempoMap":null,"phrasesMs":[],"phraseBars":16},"harmony":{"camelot":"8A","key":"A","scale":"minor","confidence":0},"sections":[{"type":"intro","startMs":0,"endMs":1000,"energy":1,"beatStart":0,"beatEnd":0}],"energy":null,"onsets":null,"vocals":null,"mixRegions":{"introStartMs":0,"introEndMs":0,"outroStartMs":0,"outroEndMs":0},"loudness":{"integratedLufs":-70,"peakAmplitude":0.25},"vector":[0,0.5],"analysisVersion":3,"analyzerVersion":"cadence/1"}"#
        );
        let back: Fingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fp);
    }
}
