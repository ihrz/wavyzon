//! Signal-level building blocks: FFT, tempo, key, meter, phrase grid and the
//! vocal heuristics. Everything here is a pure function over plain slices so
//! it can be tested with synthetic signals.

use std::f64::consts::PI;

use crate::fingerprint::Harmony;

/// Returned by [`fft`] when the input length is not a power of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fft size must be a power of two")]
pub struct FftSizeError;

/// In-place iterative radix-2 FFT over separate real/imaginary buffers.
///
/// Both slices must have the same power-of-two length.
pub fn fft(re: &mut [f64], im: &mut [f64]) -> Result<(), FftSizeError> {
    let n = re.len();
    if n & n.wrapping_sub(1) != 0 || im.len() != n {
        return Err(FftSizeError);
    }
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut size = 2;
    while size <= n {
        let ang = -2.0 * PI / size as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = size / 2;
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..half {
                let (a, b) = (i + k, i + k + half);
                let vr = re[b] * cr - im[b] * ci;
                let vi = re[b] * ci + im[b] * cr;
                re[b] = re[a] - vr;
                im[b] = im[a] - vi;
                re[a] += vr;
                im[a] += vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += size;
        }
        size <<= 1;
    }
    Ok(())
}

/// A tempo estimate over an onset envelope.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tempo {
    /// Beats per minute, `0` when no tempo was found.
    pub bpm: f64,
    /// How much the winning lag stands out from the average, `0..=1`.
    pub confidence: f64,
    /// Beat period in envelope frames (fractional).
    pub period_frames: f64,
    /// Envelope frame of the first beat.
    pub phase_frames: f64,
}

const MIN_BPM: f64 = 60.0;
const MAX_BPM: f64 = 200.0;
const PREFERRED_MIN_BPM: f64 = 90.0;
const PREFERRED_MAX_BPM: f64 = 180.0;

pub(crate) fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

pub(crate) fn max_of(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

pub(crate) fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    lo.max(hi.min(v))
}

pub(crate) fn floor_mod(a: i64, b: i64) -> i64 {
    a.rem_euclid(b)
}

fn autocorr(env: &[f64], lag: usize) -> f64 {
    let sum: f64 = (lag..env.len()).map(|i| env[i] * env[i - lag]).sum();
    sum / (env.len() - lag) as f64
}

/// Estimate the tempo of an onset-strength `envelope` sampled at `frame_rate`
/// frames per second, by autocorrelation between 60 and 200 BPM, folded into
/// the 90–180 BPM range.
pub fn estimate_tempo(envelope: &[f64], frame_rate: f64) -> Tempo {
    if envelope.len() < 16 {
        return Tempo::default();
    }
    let m = mean(envelope);
    let centered: Vec<f64> = envelope.iter().map(|v| (v - m).max(0.0)).collect();
    let min_lag = 2.max((60.0 * frame_rate / MAX_BPM) as usize);
    let max_lag = (centered.len() / 2).min((60.0 * frame_rate / MIN_BPM) as usize);
    if max_lag <= min_lag {
        return Tempo::default();
    }
    let (mut best_lag, mut best_score, mut score_sum) = (min_lag, f64::NEG_INFINITY, 0.0);
    for lag in min_lag..=max_lag {
        let s = autocorr(&centered, lag);
        score_sum += s;
        if s > best_score {
            best_score = s;
            best_lag = lag;
        }
    }
    let avg = score_sum / (max_lag - min_lag + 1) as f64;
    let confidence = if best_score > 0.0 {
        clamp((best_score - avg) / best_score, 0.0, 1.0)
    } else {
        0.0
    };
    let mut period = refine_period(&centered, best_lag);
    let mut bpm = if period > 0.0 { 60.0 * frame_rate / period } else { 0.0 };
    while (0.1..=PREFERRED_MIN_BPM).contains(&bpm) && bpm * 2.0 <= PREFERRED_MAX_BPM {
        bpm *= 2.0;
        period /= 2.0;
    }
    while bpm > PREFERRED_MAX_BPM {
        bpm /= 2.0;
        period *= 2.0;
    }
    Tempo {
        bpm,
        confidence,
        period_frames: period,
        phase_frames: beat_phase(&centered, period),
    }
}

fn refine_period(env: &[f64], lag: usize) -> f64 {
    if lag <= 1 || lag + 1 >= env.len() {
        return lag as f64;
    }
    let (prev, cur, next) = (autocorr(env, lag - 1), autocorr(env, lag), autocorr(env, lag + 1));
    let denom = prev - 2.0 * cur + next;
    if denom == 0.0 {
        return lag as f64;
    }
    let offset = 0.5 * (prev - next) / denom;
    if offset > -1.0 && offset < 1.0 {
        lag as f64 + offset
    } else {
        lag as f64
    }
}

fn beat_phase(env: &[f64], period: f64) -> f64 {
    if period <= 0.0 {
        return 0.0;
    }
    let (mut best, mut best_sum) = (0.0, f64::NEG_INFINITY);
    for step in 0..1.max(period as usize) {
        let mut sum = 0.0;
        let mut pos = step as f64;
        while pos < env.len() as f64 {
            sum += env[pos as usize];
            pos += period;
        }
        if sum > best_sum {
            best_sum = sum;
            best = step as f64;
        }
    }
    best
}

const MAJOR_PROFILE: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const MINOR_PROFILE: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];
const PITCH_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const MAJOR_CAMELOT: [&str; 12] = [
    "8B", "3B", "10B", "5B", "12B", "7B", "2B", "9B", "4B", "11B", "6B", "1B",
];
const MINOR_CAMELOT: [&str; 12] = [
    "5A", "12A", "7A", "2A", "9A", "4A", "11A", "6A", "1A", "8A", "3A", "10A",
];

fn correlate(chroma: &[f64; 12], profile: &[f64; 12], rotation: usize) -> f64 {
    let cm = chroma.iter().sum::<f64>() / 12.0;
    let pm = profile.iter().sum::<f64>() / 12.0;
    let (mut num, mut cv, mut pv) = (0.0, 0.0, 0.0);
    for i in 0..12 {
        let c = chroma[(i + rotation) % 12] - cm;
        let p = profile[i] - pm;
        num += c * p;
        cv += c * c;
        pv += p * p;
    }
    let denom = (cv * pv).sqrt();
    if denom <= 0.0 { 0.0 } else { num / denom }
}

/// Detect the key of a 12-bin chroma vector (index 0 = C) with
/// Krumhansl–Schmuckler profiles. An empty chroma falls back to `8A` (A minor).
pub fn detect_key(chroma: &[f64; 12]) -> Harmony {
    let total: f64 = chroma.iter().sum();
    if total <= 0.0 {
        return Harmony {
            camelot: "8A".into(),
            key: "A".into(),
            scale: "minor".into(),
            confidence: 0.0,
        };
    }
    let mut norm = [0.0; 12];
    for (n, c) in norm.iter_mut().zip(chroma) {
        *n = c / total;
    }
    let (mut best, mut second) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let (mut best_pc, mut best_major) = (0usize, true);
    for pc in 0..12 {
        for major in [true, false] {
            let profile = if major { &MAJOR_PROFILE } else { &MINOR_PROFILE };
            let s = correlate(&norm, profile, pc);
            if s > best {
                second = best;
                best = s;
                best_pc = pc;
                best_major = major;
            } else if s > second {
                second = s;
            }
        }
    }
    let margin = if best > 0.0 && second != f64::NEG_INFINITY {
        clamp((best - second) / best, 0.0, 1.0)
    } else {
        0.0
    };
    let confidence = clamp(clamp(best, 0.0, 1.0) * 0.5 + margin * 0.5, 0.0, 1.0);
    let (camelot, scale) = if best_major {
        (MAJOR_CAMELOT[best_pc], "major")
    } else {
        (MINOR_CAMELOT[best_pc], "minor")
    };
    Harmony {
        camelot: camelot.into(),
        key: PITCH_NAMES[best_pc].into(),
        scale: scale.into(),
        confidence,
    }
}

/// Time signature and bar alignment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    /// 3 or 4.
    pub beats_per_bar: i64,
    /// Index of the first downbeat in the beat list.
    pub phase: i64,
    /// Accent contrast between downbeats and other beats.
    pub confidence: f64,
}

const PREFER_FOUR: f64 = 1.1;
const MIN_METER_BEATS: usize = 8;

/// Pick 4/4 or 3/4 (and the downbeat phase) from per-beat accent strengths.
/// Falls back to 4/4 with phase 0.
pub fn detect_meter(accents: &[f64]) -> Meter {
    let fallback = Meter {
        beats_per_bar: 4,
        phase: 0,
        confidence: 0.0,
    };
    if accents.len() < MIN_METER_BEATS {
        return fallback;
    }
    let mut best = fallback;
    for candidate in [4, 3] {
        let m = meter_phase(accents, candidate);
        let weighted = if candidate == 4 {
            m.confidence * PREFER_FOUR
        } else {
            m.confidence
        };
        if weighted > best.confidence {
            best = Meter {
                beats_per_bar: candidate,
                phase: m.phase,
                confidence: weighted,
            };
        }
    }
    if best.confidence <= 0.0 { fallback } else { best }
}

/// Envelope strength around each beat (max over ±1 frame).
pub fn accents_at_beats(envelope: &[f64], beats_ms: &[i64], frame_rate: f64) -> Vec<f64> {
    if envelope.is_empty() || beats_ms.is_empty() {
        return Vec::new();
    }
    beats_ms
        .iter()
        .map(|&b| {
            let centre = (b as f64 / 1000.0 * frame_rate) as i64;
            let mut out = 0.0f64;
            for off in -1..=1 {
                let at = centre + off;
                if at >= 0 && (at as usize) < envelope.len() && envelope[at as usize] > out {
                    out = envelope[at as usize];
                }
            }
            out
        })
        .collect()
}

fn meter_phase(accents: &[f64], beats_per_bar: i64) -> Meter {
    let total = mean(accents);
    if total <= 0.0 {
        return Meter {
            beats_per_bar,
            phase: 0,
            confidence: 0.0,
        };
    }
    let (mut best_phase, mut best_contrast) = (0, f64::NEG_INFINITY);
    for phase in 0..beats_per_bar {
        let (mut on, mut on_n, mut off, mut off_n) = (0.0, 0, 0.0, 0);
        for (i, a) in accents.iter().enumerate() {
            if floor_mod(i as i64 - phase, beats_per_bar) == 0 {
                on += a;
                on_n += 1;
            } else {
                off += a;
                off_n += 1;
            }
        }
        if on_n == 0 || off_n == 0 {
            continue;
        }
        let c = on / on_n as f64 - off / off_n as f64;
        if c > best_contrast {
            best_contrast = c;
            best_phase = phase;
        }
    }
    Meter {
        beats_per_bar,
        phase: best_phase,
        confidence: clamp(best_contrast / total, 0.0, 1.0),
    }
}

/// Phrase length used when sections line up on 16-bar boundaries.
pub const LONG_PHRASE_BARS: i64 = 16;
/// Phrase length used when sections line up on 8-bar boundaries.
pub const SHORT_PHRASE_BARS: i64 = 8;
const LONG_PHRASE_SHARE: f64 = 0.75;

/// The phrase grid laid over the downbeats.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Phrases {
    /// Bars per phrase (8 or 16).
    pub bars: i64,
    /// Index of the downbeat the grid starts on.
    pub phase_bar: i64,
    /// Start time of each phrase.
    pub phrases_ms: Vec<i64>,
}

pub(crate) fn nearest_index(values: &[i64], target: i64) -> usize {
    let (mut best, mut delta) = (0, (values[0] - target).abs());
    for (i, v) in values.iter().enumerate() {
        let d = (v - target).abs();
        if d < delta {
            best = i;
            delta = d;
        }
    }
    best
}

/// Fit an 8- or 16-bar phrase grid to the downbeats so that it agrees with
/// as many section boundaries as possible (16 bars preferred).
pub fn detect_phrases(downbeats_ms: &[i64], boundaries_ms: &[i64]) -> Phrases {
    if downbeats_ms.is_empty() {
        return Phrases {
            bars: LONG_PHRASE_BARS,
            phase_bar: 0,
            phrases_ms: Vec::new(),
        };
    }
    let bars: Vec<i64> = boundaries_ms
        .iter()
        .map(|&b| nearest_index(downbeats_ms, b) as i64)
        .collect();
    let (long_phase, long_score) = phrase_phase(&bars, LONG_PHRASE_BARS);
    let (short_phase, short_score) = phrase_phase(&bars, SHORT_PHRASE_BARS);
    let (length, phase) = if short_score == 0 || long_score as f64 >= short_score as f64 * LONG_PHRASE_SHARE {
        (LONG_PHRASE_BARS, long_phase)
    } else {
        (SHORT_PHRASE_BARS, short_phase)
    };
    let phrases_ms = downbeats_ms
        .iter()
        .skip(phase as usize)
        .step_by(length as usize)
        .copied()
        .collect();
    Phrases {
        bars: length,
        phase_bar: phase,
        phrases_ms,
    }
}

fn phrase_phase(bars: &[i64], length: i64) -> (i64, i64) {
    if bars.is_empty() {
        return (0, 0);
    }
    let (mut best, mut best_score) = (0, -1);
    for phase in 0..length {
        let score = bars.iter().filter(|&&b| floor_mod(b - phase, length) == 0).count() as i64;
        if score > best_score {
            best = phase;
            best_score = score;
        }
    }
    (best, best_score)
}

const VOCAL_LOW_HZ: f64 = 200.0;
const VOCAL_HIGH_HZ: f64 = 4000.0;
const MIN_F0_HZ: f64 = 80.0;
const MAX_F0_HZ: f64 = 1000.0;
const VIBRATO_LOW: f64 = 4.0;
const VIBRATO_HIGH: f64 = 7.0;
const HARMONICS: usize = 4;
const SEMITONE: f64 = 100.0;
const MAX_VIBRATO_FRAMES: usize = 1024;

/// Share of the spectrum that is centre-panned (mid minus side) in the vocal
/// band, `0..=1`.
pub fn vocal_ratio(mid: &[f64], side: &[f64], bin_hz: f64) -> f64 {
    let (mut centre, mut total) = (0.0, 0.0);
    for (b, (&m, &s)) in mid.iter().zip(side).enumerate() {
        let hz = b as f64 * bin_hz;
        total += m;
        if (VOCAL_LOW_HZ..=VOCAL_HIGH_HZ).contains(&hz) {
            centre += (m - s).max(0.0);
        }
    }
    if total <= 0.0 {
        0.0
    } else {
        clamp(centre / total, 0.0, 1.0)
    }
}

/// Dominant fundamental (80–1000 Hz) by harmonic product spectrum, `0` when
/// none.
pub fn dominant_f0(mag: &[f64], bin_hz: f64) -> f64 {
    if mag.is_empty() || bin_hz <= 0.0 {
        return 0.0;
    }
    let low = 1i64.max((MIN_F0_HZ / bin_hz) as i64);
    let high = (mag.len() as i64 / HARMONICS as i64 - 1).min((MAX_F0_HZ / bin_hz) as i64);
    if high <= low {
        return 0.0;
    }
    let (mut best_bin, mut best_score) = (0usize, 0.0);
    for b in low as usize..=high as usize {
        let mut product = mag[b];
        let mut h = 2;
        while h <= HARMONICS && b * h < mag.len() {
            product *= mag[b * h];
            h += 1;
        }
        if product > best_score {
            best_score = product;
            best_bin = b;
        }
    }
    if best_score <= 0.0 {
        0.0
    } else {
        best_bin as f64 * bin_hz
    }
}

fn cents(from: f64, to: f64) -> f64 {
    1200.0 * (to / from).log2()
}

/// Share of consecutive voiced frames in `from..to` whose pitch moves by at
/// most a semitone.
pub fn continuity(f0: &[f64], from: usize, to: usize) -> f64 {
    if to < from + 2 {
        return 0.0;
    }
    let (mut steady, mut compared) = (0, 0);
    for i in from.max(1)..to.min(f0.len()) {
        if f0[i - 1] <= 0.0 || f0[i] <= 0.0 {
            continue;
        }
        compared += 1;
        if cents(f0[i - 1], f0[i]).abs() <= SEMITONE {
            steady += 1;
        }
    }
    if compared == 0 {
        0.0
    } else {
        steady as f64 / compared as f64
    }
}

/// Share of the pitch contour's modulation energy in the 4–7 Hz vibrato band.
pub fn vibrato_strength(f0: &[f64], frame_rate: f64, from: usize, to: usize) -> f64 {
    let start = from;
    let end = to.min(f0.len()).min(start + MAX_VIBRATO_FRAMES);
    if end < start + 8 || frame_rate <= 0.0 {
        return 0.0;
    }
    let length = end - start;
    let mut contour = vec![0.0; length];
    let (mut sum, mut voiced) = (0.0, 0);
    for i in 0..length {
        let v = f0[start + i];
        if v > 0.0 {
            contour[i] = 1200.0 * (v / MIN_F0_HZ).log2();
            sum += contour[i];
            voiced += 1;
        }
    }
    if voiced < length / 2 {
        return 0.0;
    }
    let m = sum / voiced as f64;
    let (mut band, mut total) = (0.0, 0.0);
    let step = frame_rate / length as f64;
    for k in 1..length / 2 {
        let hz = k as f64 * step;
        let (mut re, mut im) = (0.0, 0.0);
        for n in 0..length {
            if f0[start + n] <= 0.0 {
                continue;
            }
            let angle = -2.0 * PI * k as f64 * n as f64 / length as f64;
            let c = contour[n] - m;
            re += c * angle.cos();
            im += c * angle.sin();
        }
        let p = re * re + im * im;
        total += p;
        if (VIBRATO_LOW..=VIBRATO_HIGH).contains(&hz) {
            band += p;
        }
    }
    if total <= 0.0 {
        0.0
    } else {
        clamp(band / total, 0.0, 1.0)
    }
}

/// Direct-form-I biquad section.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    pub(crate) fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            ..Self::default()
        }
    }

    pub(crate) fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_tone() {
        let n = 1024;
        let mut re: Vec<f64> = (0..n).map(|i| (2.0 * PI * 32.0 * i as f64 / n as f64).sin()).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im).unwrap();
        let mut best = 0;
        for b in 1..n / 2 {
            if re[b].hypot(im[b]) > re[best].hypot(im[best]) {
                best = b;
            }
        }
        assert_eq!(best, 32);
        assert!(fft(&mut [0.0; 3], &mut [0.0; 3]).is_err(), "non power of two accepted");
    }

    #[test]
    fn tempo_from_pulse_train() {
        let rate = 93.75;
        let mut env = vec![0.0; (rate * 30.0) as usize];
        let period = 60.0 * rate / 128.0;
        let mut p = 0.0;
        while p < env.len() as f64 {
            env[p as usize] = 1.0;
            p += period;
        }
        let tempo = estimate_tempo(&env, rate);
        assert!((tempo.bpm - 128.0).abs() <= 2.0, "bpm {:.2}", tempo.bpm);
    }

    #[test]
    fn tempo_folds_slow_pulses_up() {
        let rate = 93.75;
        let mut env = vec![0.0; (rate * 40.0) as usize];
        let period = 60.0 * rate / 70.0;
        let mut p = 0.0;
        while p < env.len() as f64 {
            env[p as usize] = 1.0;
            p += period;
        }
        let tempo = estimate_tempo(&env, rate);
        assert!((tempo.bpm - 140.0).abs() <= 3.0, "bpm {:.2}", tempo.bpm);
        assert_eq!(estimate_tempo(&[0.0; 8], rate), Tempo::default());
    }

    #[test]
    fn key_from_c_major_chroma() {
        let mut chroma = [0.0; 12];
        for pc in [0, 4, 7, 0, 2, 5, 9, 11, 0, 7] {
            chroma[pc] += 1.0;
        }
        let h = detect_key(&chroma);
        assert_eq!(
            (h.key.as_str(), h.scale.as_str(), h.camelot.as_str()),
            ("C", "major", "8B"),
            "{h:?}"
        );
        assert_eq!(detect_key(&[0.0; 12]).camelot, "8A", "empty chroma fallback");
    }

    #[test]
    fn key_from_a_minor_chroma() {
        let mut chroma = [0.0; 12];
        for pc in [9, 0, 4, 9, 11, 2, 5, 7, 9, 4] {
            chroma[pc] += 1.0;
        }
        let h = detect_key(&chroma);
        assert_eq!(
            (h.key.as_str(), h.scale.as_str(), h.camelot.as_str()),
            ("A", "minor", "8A"),
            "{h:?}"
        );
    }

    #[test]
    fn meter_finds_bar_phase() {
        let accents: Vec<f64> = (0..64).map(|i| if (i - 1) % 4 == 0 { 1.0 } else { 0.3 }).collect();
        let m = detect_meter(&accents);
        assert_eq!((m.beats_per_bar, m.phase), (4, 1), "{m:?}");
        let w: Vec<f64> = (0..60).map(|i| if i % 3 == 2 { 1.0 } else { 0.2 }).collect();
        let m = detect_meter(&w);
        assert_eq!((m.beats_per_bar, m.phase), (3, 2), "waltz {m:?}");
        assert_eq!(detect_meter(&[1.0; 4]).beats_per_bar, 4);
    }

    #[test]
    fn phrase_grid_picks_sixteen_bars() {
        let downbeats: Vec<i64> = (0..128).map(|i| i * 2000).collect();
        let p = detect_phrases(&downbeats, &[4 * 2000, 20 * 2000, 36 * 2000, 52 * 2000]);
        assert_eq!((p.bars, p.phase_bar, p.phrases_ms[0]), (16, 4, 8000), "{p:?}");
    }

    #[test]
    fn phrase_grid_picks_eight_bars() {
        let downbeats: Vec<i64> = (0..128).map(|i| i * 2000).collect();
        let p = detect_phrases(&downbeats, &[2 * 2000, 10 * 2000, 18 * 2000, 26 * 2000]);
        assert_eq!((p.bars, p.phase_bar), (8, 2), "{p:?}");
    }

    #[test]
    fn f0_and_vibrato() {
        let bin_hz = 48000.0 / 2048.0;
        let mut mag = vec![0.0; 1024];
        let f = 10; // ~234 Hz
        for h in 1..=4 {
            mag[f * h] = 1.0;
        }
        assert!((dominant_f0(&mag, bin_hz) - f as f64 * bin_hz).abs() < 1e-9);
        let rate = 93.75;
        let wobble: Vec<f64> = (0..400)
            .map(|i| 220.0 * (1.0 + 0.02 * (2.0 * PI * 5.5 * i as f64 / rate).sin()))
            .collect();
        assert!(vibrato_strength(&wobble, rate, 0, wobble.len()) > 0.5);
        assert!(continuity(&wobble, 0, wobble.len()) > 0.99);
    }
}
