//! The streaming analyser: fed 20 ms frames (live while a track plays, or
//! offline from a decoder), it produces a [`Fingerprint`] when finished.

use std::f64::consts::PI;
use std::sync::{Arc, LazyLock};

use wavyzon_audio::{CHANNELS, Frame, SAMPLE_RATE};

use crate::dsp::{
    Tempo, accents_at_beats, clamp, continuity, detect_key, detect_meter, detect_phrases, dominant_f0, estimate_tempo,
    fft, floor_mod, max_of, mean, vibrato_strength, vocal_ratio,
};
use crate::fingerprint::{
    ANALYSIS_VERSION, ANALYZER_VERSION, EnergyPoint, Fingerprint, LoudnessResult, Onset, Registry, Rhythm, TempoPoint,
    VocalActivity,
};
use crate::structure::{Loudness, detect_mix_regions, segment};

const WINDOW: usize = 2048;
const HOP: usize = 512;
const BINS: usize = WINDOW / 2;
const TIMBRE_BANDS: usize = 8;
const FEATURE_SIZE: usize = 12 + TIMBRE_BANDS;
const FRAMES_PER_FEATURE: usize = 47;
const MIN_VOCAL_RATIO: f64 = 0.12;
const MIN_VOCAL_MS: i64 = 1000;
const MIN_CONTINUITY: f64 = 0.6;
const MIN_VIBRATO: f64 = 0.25;
const MAX_BEATS: usize = 20000;
const CHROMA_LOW_HZ: f64 = 65.0;
const CHROMA_HIGH_HZ: f64 = 5000.0;

const FRAME_RATE: f64 = SAMPLE_RATE as f64 / HOP as f64;
const BIN_HZ: f64 = SAMPLE_RATE as f64 / WINDOW as f64;

static HANN: LazyLock<Vec<f64>> = LazyLock::new(|| {
    (0..WINDOW)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / (WINDOW - 1) as f64).cos())
        .collect()
});

/// Pitch class of each FFT bin inside the chroma range, `None` outside it.
static BIN_PITCH: LazyLock<Vec<Option<usize>>> = LazyLock::new(|| {
    (0..BINS)
        .map(|b| {
            let hz = b as f64 * BIN_HZ;
            if (CHROMA_LOW_HZ..=CHROMA_HIGH_HZ).contains(&hz) {
                pitch_class(hz)
            } else {
                None
            }
        })
        .collect()
});

fn pitch_class(hz: f64) -> Option<usize> {
    let midi = 69.0 + 12.0 * (hz / 440.0).log2();
    if midi.is_nan() || midi < 0.0 {
        return None;
    }
    Some(floor_mod(midi.round() as i64, 12) as usize)
}

fn to_ms(frame: f64) -> i64 {
    (frame / FRAME_RATE * 1000.0) as i64
}

/// Called once with the finished fingerprint.
pub type OnDone = Box<dyn FnOnce(Arc<Fingerprint>) + Send>;

/// Streaming track analyser.
///
/// Feed it 48 kHz stereo [`Frame`]s with [`feed`](Self::feed); call
/// [`finish`](Self::finish) when the track ends (it is idempotent) or
/// [`result`](Self::result) to compute the fingerprint directly. This is the
/// live "tap" the player attaches to a deck.
pub struct Analyzer {
    source: String,
    identifier: String,
    on_done: Option<OnDone>,

    mid: Vec<f64>,
    side: Vec<f64>,
    filled: usize,
    re: Vec<f64>,
    im: Vec<f64>,
    sre: Vec<f64>,
    sim: Vec<f64>,
    mag: Vec<f64>,
    side_mag: Vec<f64>,
    prev: Vec<f64>,

    flux: Vec<f64>,
    rms: Vec<f64>,
    centroid: Vec<f64>,
    vocal: Vec<f64>,
    f0: Vec<f64>,
    chroma: [f64; 12],
    features: Vec<Vec<f64>>,
    acc: [f64; FEATURE_SIZE],
    acc_count: usize,
    loud: Loudness,
    samples: i64,

    finished: Option<Arc<Fingerprint>>,
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analyzer")
            .field("source", &self.source)
            .field("identifier", &self.identifier)
            .field("samples", &self.samples)
            .field("finished", &self.finished.is_some())
            .finish_non_exhaustive()
    }
}

impl Analyzer {
    /// An analyser for `source:identifier`; `on_done` runs once, on the first
    /// [`finish`](Self::finish).
    pub fn new(source: impl Into<String>, identifier: impl Into<String>, on_done: Option<OnDone>) -> Self {
        Self {
            source: source.into(),
            identifier: identifier.into(),
            on_done,
            mid: vec![0.0; WINDOW],
            side: vec![0.0; WINDOW],
            filled: 0,
            re: vec![0.0; WINDOW],
            im: vec![0.0; WINDOW],
            sre: vec![0.0; WINDOW],
            sim: vec![0.0; WINDOW],
            mag: vec![0.0; BINS],
            side_mag: vec![0.0; BINS],
            prev: vec![0.0; BINS],
            flux: Vec::new(),
            rms: Vec::new(),
            centroid: Vec::new(),
            vocal: Vec::new(),
            f0: Vec::new(),
            chroma: [0.0; 12],
            features: Vec::new(),
            acc: [0.0; FEATURE_SIZE],
            acc_count: 0,
            loud: Loudness::new(),
            samples: 0,
            finished: None,
        }
    }

    /// A live tap that stores its fingerprint in `registry` when finished.
    pub fn storing_into(source: impl Into<String>, identifier: impl Into<String>, registry: Arc<Registry>) -> Self {
        Self::new(source, identifier, Some(Box::new(move |fp| registry.store(fp))))
    }

    /// Samples (per channel) analysed so far.
    pub fn samples(&self) -> i64 {
        self.samples
    }

    /// The loudness meter (live integrated LUFS and peak so far).
    pub fn loudness(&self) -> &Loudness {
        &self.loud
    }

    /// Analyse one 20 ms frame. Ignored once a result was computed.
    pub fn feed(&mut self, f: &Frame) {
        self.feed_samples(f);
    }

    /// Analyse any number of interleaved 48 kHz stereo samples.
    pub fn feed_samples(&mut self, f: &[i16]) {
        if self.finished.is_some() {
            return;
        }
        self.loud.feed_samples(f);
        for pair in f.chunks_exact(2) {
            let (l, r) = (pair[0] as f64 / 32768.0, pair[1] as f64 / 32768.0);
            self.mid[self.filled] = (l + r) * 0.5;
            self.side[self.filled] = (l - r) * 0.5;
            self.filled += 1;
            if self.filled == WINDOW {
                self.process();
                self.mid.copy_within(HOP.., 0);
                self.side.copy_within(HOP.., 0);
                self.filled = WINDOW - HOP;
            }
        }
        self.samples += (f.len() / CHANNELS) as i64;
    }

    // Several parallel buffers are indexed per bin; an index loop is clearest.
    #[allow(clippy::needless_range_loop)]
    fn process(&mut self) {
        let hann = &*HANN;
        for i in 0..WINDOW {
            self.re[i] = self.mid[i] * hann[i];
            self.im[i] = 0.0;
            self.sre[i] = self.side[i] * hann[i];
            self.sim[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im).expect("power-of-two window");
        fft(&mut self.sre, &mut self.sim).expect("power-of-two window");
        let pitch = &*BIN_PITCH;
        let (mut energy, mut weighted, mut mag_sum, mut flux_sum) = (0.0, 0.0, 0.0, 0.0);
        for b in 0..BINS {
            let m = self.re[b].hypot(self.im[b]);
            self.mag[b] = m;
            self.side_mag[b] = self.sre[b].hypot(self.sim[b]);
            energy += m * m;
            mag_sum += m;
            let hz = b as f64 * BIN_HZ;
            weighted += hz * m;
            let d = m - self.prev[b];
            if d > 0.0 {
                flux_sum += d;
            }
            if let (true, Some(pc)) = (m > 0.0, pitch[b]) {
                self.chroma[pc] += m;
            }
        }
        self.prev.copy_from_slice(&self.mag);
        self.flux.push(flux_sum);
        self.rms.push((energy / BINS as f64).sqrt());
        self.centroid.push(if mag_sum > 0.0 { weighted / mag_sum } else { 0.0 });
        self.vocal.push(vocal_ratio(&self.mag, &self.side_mag, BIN_HZ));
        self.f0.push(dominant_f0(&self.mag, BIN_HZ));
        self.accumulate(mag_sum);
    }

    fn accumulate(&mut self, mag_sum: f64) {
        if mag_sum > 0.0 {
            let pitch = &*BIN_PITCH;
            for (m, pc) in self.mag.iter().zip(pitch) {
                if let Some(pc) = *pc {
                    self.acc[pc] += m / mag_sum;
                }
            }
            for band in 0..TIMBRE_BANDS {
                let (from, to) = (BINS * band / TIMBRE_BANDS, BINS * (band + 1) / TIMBRE_BANDS);
                let sum: f64 = self.mag[from..to].iter().sum();
                self.acc[12 + band] += sum / mag_sum;
            }
        }
        self.acc_count += 1;
        if self.acc_count < FRAMES_PER_FEATURE {
            return;
        }
        let n = self.acc_count as f64;
        self.features.push(self.acc.iter().map(|a| a / n).collect());
        self.acc = [0.0; FEATURE_SIZE];
        self.acc_count = 0;
    }

    /// Compute the fingerprint (once) and hand it to the `on_done` callback.
    /// Later calls do nothing.
    pub fn finish(&mut self) {
        let fp = self.result();
        if let Some(done) = self.on_done.take() {
            done(fp);
        }
    }

    /// The fingerprint of everything fed so far. The first call freezes the
    /// analyser: later frames are ignored and the same result is returned.
    pub fn result(&mut self) -> Arc<Fingerprint> {
        if let Some(fp) = &self.finished {
            return fp.clone();
        }
        let fp = Arc::new(self.compute());
        tracing::debug!(
            track = %fp.key(),
            bpm = fp.rhythm.bpm,
            key = %fp.harmony.camelot,
            lufs = fp.loudness.integrated_lufs,
            duration_ms = fp.duration_ms,
            "track analysed"
        );
        self.finished = Some(fp.clone());
        fp
    }

    fn compute(&self) -> Fingerprint {
        let duration_ms = self.samples * 1000 / SAMPLE_RATE as i64;
        let tempo = estimate_tempo(&self.flux, FRAME_RATE);
        let beats = beats(&tempo, duration_ms);
        let meter = detect_meter(&accents_at_beats(&self.flux, &beats, FRAME_RATE));
        let downbeats: Vec<i64> = beats
            .iter()
            .enumerate()
            .filter(|(i, _)| floor_mod(*i as i64 - meter.phase, meter.beats_per_bar) == 0)
            .map(|(_, b)| *b)
            .collect();
        let harmony = detect_key(&self.chroma);
        let energy = self.energy();
        let ms_per_feature = (FRAMES_PER_FEATURE as f64 * 1000.0 / FRAME_RATE).round() as i64;
        let sections = segment(&energy, &beats, duration_ms, &self.features, ms_per_feature);
        let vocals = self.vocals();
        let boundaries: Vec<i64> = sections.iter().skip(1).map(|s| s.start_ms).collect();
        let phrases = detect_phrases(&downbeats, &boundaries);
        let tempo_map = if tempo.bpm > 0.0 {
            vec![TempoPoint {
                time_ms: 0,
                bpm: tempo.bpm,
            }]
        } else {
            Vec::new()
        };
        let mix_regions = detect_mix_regions(&energy, &vocals, &phrases.phrases_ms, duration_ms);
        Fingerprint {
            source: self.source.clone(),
            identifier: self.identifier.clone(),
            duration_ms,
            rhythm: Rhythm {
                bpm: tempo.bpm,
                bpm_confidence: tempo.confidence,
                meter: meter.beats_per_bar,
                beats_ms: beats,
                downbeats_ms: downbeats,
                tempo_map,
                phrases_ms: phrases.phrases_ms,
                phrase_bars: phrases.bars,
            },
            harmony,
            sections,
            onsets: self.onsets(),
            vocals,
            mix_regions,
            energy,
            loudness: LoudnessResult {
                integrated_lufs: self.loud.integrated(),
                peak_amplitude: self.loud.peak(),
            },
            vector: self.vector(&tempo),
            analysis_version: ANALYSIS_VERSION,
            analyzer_version: ANALYZER_VERSION.into(),
        }
    }

    fn energy(&self) -> Vec<EnergyPoint> {
        if self.rms.is_empty() {
            return Vec::new();
        }
        let per = 1.max((FRAME_RATE / 2.0) as usize);
        let peak = max_of(&self.rms).max(1e-6);
        self.rms
            .chunks(per)
            .enumerate()
            .map(|(c, chunk)| EnergyPoint {
                time_ms: to_ms((c * per) as f64),
                value: clamp(mean(chunk) / peak, 0.0, 1.0),
            })
            .collect()
    }

    fn onsets(&self) -> Vec<Onset> {
        let flux = &self.flux;
        if flux.is_empty() {
            return Vec::new();
        }
        let threshold = mean(flux) * 1.5;
        let peak = max_of(flux).max(1e-6);
        (1..flux.len().saturating_sub(1))
            .filter(|&i| flux[i] > threshold && flux[i] >= flux[i - 1] && flux[i] > flux[i + 1])
            .map(|i| Onset {
                time_ms: to_ms(i as f64),
                strength: clamp(flux[i] / peak, 0.0, 1.0),
            })
            .collect()
    }

    fn vocals(&self) -> Vec<VocalActivity> {
        if self.vocal.is_empty() {
            return Vec::new();
        }
        let threshold = MIN_VOCAL_RATIO.max(mean(&self.vocal) * 1.1);
        let mut spans = Vec::new();
        let flush = |spans: &mut Vec<VocalActivity>, start: usize, to: usize, acc: f64, count: usize| {
            if count == 0 {
                return;
            }
            let (s, e) = (to_ms(start as f64), to_ms(to as f64));
            if e - s < MIN_VOCAL_MS {
                return;
            }
            let cont = continuity(&self.f0, start, to);
            let vib = vibrato_strength(&self.f0, FRAME_RATE, start, to);
            if cont < MIN_CONTINUITY && vib < MIN_VIBRATO {
                return;
            }
            let band = clamp(acc / count as f64, 0.0, 1.0);
            spans.push(VocalActivity {
                start_ms: s,
                end_ms: e,
                confidence: clamp(band * 0.5 + cont * 0.35 + vib * 0.15, 0.0, 1.0),
            });
        };
        let (mut start, mut acc, mut count) = (None::<usize>, 0.0, 0usize);
        for (i, &v) in self.vocal.iter().enumerate() {
            let active = v >= threshold;
            if active {
                if start.is_none() {
                    start = Some(i);
                    acc = 0.0;
                    count = 0;
                }
                acc += v;
                count += 1;
            } else if let Some(s) = start.take() {
                flush(&mut spans, s, i, acc, count);
            }
        }
        if let Some(s) = start {
            flush(&mut spans, s, self.vocal.len(), acc, count);
        }
        spans
    }

    fn vector(&self, t: &Tempo) -> Vec<f64> {
        let total = self.chroma.iter().sum::<f64>().max(1e-6);
        let mut v: Vec<f64> = self.chroma.iter().map(|c| c / total).collect();
        v.push(clamp(t.bpm / 200.0, 0.0, 1.0));
        v.push(clamp(mean(&self.rms), 0.0, 1.0));
        v.push(clamp(mean(&self.centroid) / 8000.0, 0.0, 1.0));
        v.push(clamp(mean(&self.vocal), 0.0, 1.0));
        v
    }
}

fn beats(t: &Tempo, duration_ms: i64) -> Vec<i64> {
    let mut out = Vec::new();
    if t.period_frames <= 0.0 {
        return out;
    }
    let mut pos = t.phase_frames;
    while out.len() <= MAX_BEATS {
        let ms = to_ms(pos);
        if ms > duration_ms {
            break;
        }
        out.push(ms);
        pos += t.period_frames;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::tests_support::{click_frames, loud_frames, tone_frames};

    #[test]
    fn analyzer_produces_complete_fingerprint() {
        let got: Arc<Mutex<Vec<Arc<Fingerprint>>>> = Arc::default();
        let sink = got.clone();
        let mut a = Analyzer::new("test", "tone", Some(Box::new(move |f| sink.lock().unwrap().push(f))));
        for f in loud_frames(0.4, 20) {
            a.feed(&f);
        }
        a.finish();
        a.finish();
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1, "on_done must run once");
        let fp = &got[0];
        assert_eq!(fp.duration_ms, 20000);
        assert_eq!(fp.analysis_version, ANALYSIS_VERSION);
        assert_eq!(fp.vector.len(), 16);
        assert!(!fp.energy.is_empty());
        let json = serde_json::to_value(&**fp).unwrap();
        for k in ["beatsMs", "downbeatsMs", "phrasesMs"] {
            assert!(json["rhythm"][k].is_array(), "rhythm.{k} must serialize as [] not null");
        }
    }

    #[test]
    fn analyzer_finds_click_tempo_and_meter() {
        let mut a = Analyzer::new("test", "clicks", None);
        for f in click_frames(120.0, 4, 30) {
            a.feed(&f);
        }
        let fp = a.result();
        assert!((fp.rhythm.bpm - 120.0).abs() <= 2.0, "bpm {}", fp.rhythm.bpm);
        assert_eq!(fp.rhythm.meter, 4);
        let beats = &fp.rhythm.beats_ms;
        assert!(beats.len() >= 55, "{} beats", beats.len());
        let gap = (beats[beats.len() - 1] - beats[0]) as f64 / (beats.len() - 1) as f64;
        assert!((gap - 500.0).abs() < 10.0, "beat gap {gap}");
        let downbeat_gap = fp.rhythm.downbeats_ms[1] - fp.rhythm.downbeats_ms[0];
        assert!((downbeat_gap - 2000).abs() < 30, "downbeat gap {downbeat_gap}");
        // Accented clicks fall on whole seconds multiple of 2 (every 4 beats).
        let d0 = fp.rhythm.downbeats_ms[0] % 2000;
        assert!(!(60..=1940).contains(&d0), "downbeat offset {d0}");
        assert!(!fp.onsets.is_empty());
        assert_eq!(fp.rhythm.phrase_bars, 16);
    }

    #[test]
    fn analyzer_finds_key_of_a_triad() {
        // A minor triad: A5, C6, E6 (high enough for the 23 Hz bins to resolve
        // semitones).
        let mut a = Analyzer::new("test", "am", None);
        for f in tone_frames(&[880.0, 1046.5, 1318.51], 10) {
            a.feed(&f);
        }
        let fp = a.result();
        assert_eq!(fp.harmony.camelot, "8A", "{:?}", fp.harmony);
        let mut c = Analyzer::new("test", "c", None);
        for f in tone_frames(&[1046.5, 1318.51, 1567.98], 10) {
            c.feed(&f);
        }
        assert_eq!(c.result().harmony.camelot, "8B", "{:?}", c.result().harmony);
    }

    #[test]
    fn result_freezes_and_feed_is_ignored() {
        let mut a = Analyzer::new("s", "i", None);
        for f in loud_frames(0.2, 2) {
            a.feed(&f);
        }
        let first = a.result();
        a.feed(&loud_frames(0.2, 1)[0]);
        assert!(Arc::ptr_eq(&first, &a.result()));
        assert_eq!(a.samples(), 2 * SAMPLE_RATE as i64);
    }

    #[test]
    fn storing_into_registry() {
        let registry = Arc::new(Registry::new());
        let mut a = Analyzer::storing_into("s", "i", registry.clone());
        for f in loud_frames(0.2, 2) {
            a.feed(&f);
        }
        assert!(!registry.has("s", "i"));
        a.finish();
        assert!(registry.loudness("s", "i").unwrap().integrated_lufs > -30.0);
    }

    #[test]
    fn silence_is_harmless() {
        let mut a = Analyzer::new("s", "silence", None);
        for _ in 0..100 {
            a.feed(&wavyzon_audio::SILENCE);
        }
        let fp = a.result();
        assert_eq!(fp.loudness.integrated_lufs, crate::FLOOR_LUFS);
        assert_eq!(fp.harmony.camelot, "8A");
        serde_json::to_string(&*fp).unwrap();
    }
}
