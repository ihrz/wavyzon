//! Track-level structure: EBU R128 loudness, intro/outro mix regions and
//! section segmentation (self-similarity novelty, falling back to energy).

use wavyzon_audio::{Frame, SAMPLE_RATE};

use crate::dsp::{Biquad, clamp, max_of, mean, nearest_index};
use crate::fingerprint::{EnergyPoint, MixRegions, Section, VocalActivity};

/// Loudness reported for silence or too-short input.
pub const FLOOR_LUFS: f64 = -70.0;
const SUB_BLOCKS_PER_SEC: u32 = 10;
const SUB_BLOCKS_PER_BLOCK: usize = 4;
const ABSOLUTE_GATE: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;
const K_WEIGHT_OFFSET: f64 = -0.691;

/// Streaming EBU R128 / ITU-R BS.1770 integrated loudness meter for 48 kHz
/// stereo, with 400 ms blocks at 75 % overlap and absolute + relative gating.
#[derive(Debug, Clone)]
pub struct Loudness {
    shelf: [Biquad; 2],
    hp: [Biquad; 2],
    sub_blocks: Vec<f64>,
    sum: f64,
    filled: u32,
    peak: f64,
}

impl Default for Loudness {
    fn default() -> Self {
        Self::new()
    }
}

impl Loudness {
    /// A meter with nothing measured yet.
    pub fn new() -> Self {
        let shelf = Biquad::new(
            1.53512485958697,
            -2.69169618940638,
            1.19839281085285,
            -1.69065929318241,
            0.73248077421585,
        );
        let hp = Biquad::new(1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621);
        Self {
            shelf: [shelf; 2],
            hp: [hp; 2],
            sub_blocks: Vec::new(),
            sum: 0.0,
            filled: 0,
            peak: 0.0,
        }
    }

    /// Measure one frame of interleaved stereo.
    pub fn feed(&mut self, f: &Frame) {
        self.feed_samples(f);
    }

    /// Measure any number of interleaved stereo samples.
    pub fn feed_samples(&mut self, f: &[i16]) {
        let per = SAMPLE_RATE / SUB_BLOCKS_PER_SEC;
        for pair in f.chunks_exact(2) {
            let (left, right) = (pair[0] as f64 / 32768.0, pair[1] as f64 / 32768.0);
            self.peak = self.peak.max(left.abs().max(right.abs()));
            let wl = self.hp[0].process(self.shelf[0].process(left));
            let wr = self.hp[1].process(self.shelf[1].process(right));
            self.sum += wl * wl + wr * wr;
            self.filled += 1;
            if self.filled == per {
                self.sub_blocks.push(self.sum);
                self.sum = 0.0;
                self.filled = 0;
            }
        }
    }

    /// Sample peak seen so far, `0..=1`.
    pub fn peak(&self) -> f64 {
        self.peak
    }

    /// Gated integrated loudness in LUFS, at least [`FLOOR_LUFS`].
    pub fn integrated(&self) -> f64 {
        if self.sub_blocks.len() < SUB_BLOCKS_PER_BLOCK {
            return FLOOR_LUFS;
        }
        let block_samples = (SAMPLE_RATE / SUB_BLOCKS_PER_SEC) as f64 * SUB_BLOCKS_PER_BLOCK as f64;
        let above: Vec<f64> = self
            .sub_blocks
            .windows(SUB_BLOCKS_PER_BLOCK)
            .map(|w| w.iter().sum::<f64>() / block_samples)
            .filter(|&p| loudness_of(p) > ABSOLUTE_GATE)
            .collect();
        if above.is_empty() {
            return FLOOR_LUFS;
        }
        let gate = loudness_of(mean(&above)) - RELATIVE_GATE_LU;
        let gated: Vec<f64> = above.into_iter().filter(|&p| loudness_of(p) > gate).collect();
        if gated.is_empty() {
            return FLOOR_LUFS;
        }
        FLOOR_LUFS.max(loudness_of(mean(&gated)))
    }
}

fn loudness_of(power: f64) -> f64 {
    if power <= 0.0 {
        f64::NEG_INFINITY
    } else {
        K_WEIGHT_OFFSET + 10.0 * power.log10()
    }
}

const SILENCE_LEVEL: f64 = 0.05;
const FULL_RATIO: f64 = 0.85;
const MAX_INTRO_SHARE: f64 = 0.3;
const MIN_OUTRO_SHARE: f64 = 0.5;
const SUSTAIN_POINTS: usize = 3;

fn clamp_int(v: i64, lo: i64, hi: i64) -> i64 {
    if hi <= lo { lo } else { lo.max(v.min(hi)) }
}

/// The grid line in `lines` closest to `t` (or `t` itself when there are
/// none).
pub fn snap(lines: &[i64], t: i64) -> i64 {
    if lines.is_empty() {
        t
    } else {
        lines[nearest_index(lines, t)]
    }
}

/// Find the intro (up to full energy or the first vocals) and outro (after
/// the last full-energy point) regions, snapped to the phrase grid.
pub fn detect_mix_regions(
    energy: &[EnergyPoint],
    vocals: &[VocalActivity],
    phrases: &[i64],
    duration_ms: i64,
) -> MixRegions {
    let whole = MixRegions {
        intro_start_ms: 0,
        intro_end_ms: 0,
        outro_start_ms: duration_ms,
        outro_end_ms: duration_ms,
    };
    if energy.is_empty() || duration_ms <= 0 {
        return whole;
    }
    let audible: Vec<usize> = (0..energy.len())
        .filter(|&i| energy[i].value >= SILENCE_LEVEL)
        .collect();
    let (Some(&first), Some(&last)) = (audible.first(), audible.last()) else {
        return whole;
    };
    if last <= first {
        return whole;
    }
    let start = energy[first].time_ms;
    let end = duration_ms.min(energy[last].time_ms);
    let mut values: Vec<f64> = energy[first..=last].iter().map(|p| p.value).collect();
    values.sort_by(f64::total_cmp);
    let full = values[values.len() / 2] * FULL_RATIO;

    let intro_end = clamp_int(
        intro_end_of(energy, vocals, first, last, full, start),
        start,
        start + (duration_ms as f64 * MAX_INTRO_SHARE) as i64,
    );
    let outro_start = clamp_int(
        outro_start_of(energy, first, last, full, end),
        (duration_ms as f64 * MIN_OUTRO_SHARE) as i64,
        end,
    );
    let si = clamp_int(snap(phrases, intro_end), start, end);
    let so = clamp_int(snap(phrases, outro_start), start, end);
    if si >= so {
        return MixRegions {
            intro_start_ms: start,
            intro_end_ms: start,
            outro_start_ms: end,
            outro_end_ms: end,
        };
    }
    MixRegions {
        intro_start_ms: start,
        intro_end_ms: si,
        outro_start_ms: so,
        outro_end_ms: end,
    }
}

fn intro_end_of(
    energy: &[EnergyPoint],
    vocals: &[VocalActivity],
    first: usize,
    last: usize,
    full: f64,
    start: i64,
) -> i64 {
    let mut loud = start;
    let mut i = first;
    while i + SUSTAIN_POINTS <= last {
        if energy[i..i + SUSTAIN_POINTS].iter().all(|p| p.value >= full) {
            loud = energy[i].time_ms;
            break;
        }
        i += 1;
    }
    match vocals.first() {
        None => loud,
        Some(v) => loud.min(start.max(v.start_ms)),
    }
}

fn outro_start_of(energy: &[EnergyPoint], first: usize, last: usize, full: f64, end: i64) -> i64 {
    for i in (first..=last).rev() {
        if energy[i].value >= full {
            return if i >= last { end } else { energy[i + 1].time_ms };
        }
    }
    end
}

/// Half-width (in feature steps) of the checkerboard novelty kernel.
pub const KERNEL_LAGS: usize = 32;
/// Shortest section the segmenter produces.
pub const MIN_SEGMENT_MS: i64 = 8000;
const NOVELTY_THRESHOLD: f64 = 1.4;
const REPEAT_THRESHOLD: f64 = 0.86;
const EDGE_ENERGY_RATIO: f64 = 0.8;
const CHORUS_REPEAT: f64 = 0.3;
const QUIET_LEVEL: f64 = 0.35;
const LOUD_LEVEL: f64 = 0.7;
const BUILD_SLOPE: f64 = 0.15;
const SMOOTH_RADIUS: usize = 4;

fn norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Cosine self-similarity matrix of feature vectors (clamped to `0..=1`).
pub fn similarity_matrix(features: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = features.len();
    let mut out = vec![vec![0.0; n]; n];
    let norms: Vec<f64> = features.iter().map(|f| norm(f)).collect();
    for i in 0..n {
        out[i][i] = 1.0;
        for j in i + 1..n {
            let mut v = 0.0;
            if norms[i] > 0.0 && norms[j] > 0.0 {
                let dot: f64 = features[i].iter().zip(&features[j]).map(|(a, b)| a * b).sum();
                v = clamp(dot / (norms[i] * norms[j]), 0.0, 1.0);
            }
            out[i][j] = v;
            out[j][i] = v;
        }
    }
    out
}

fn checkerboard(lags: usize) -> Vec<Vec<f64>> {
    let size = lags * 2;
    let sigma = lags as f64 / 2.0;
    (0..size)
        .map(|a| {
            (0..size)
                .map(|b| {
                    let x = a as f64 - lags as f64 + 0.5;
                    let y = b as f64 - lags as f64 + 0.5;
                    let sign = if (x < 0.0) == (y < 0.0) { 1.0 } else { -1.0 };
                    sign * (-(x * x + y * y) / (2.0 * sigma * sigma)).exp()
                })
                .collect()
        })
        .collect()
}

/// Foote novelty curve: a Gaussian checkerboard kernel slid along the
/// diagonal of `ssm`.
pub fn novelty(ssm: &[Vec<f64>], lags: usize) -> Vec<f64> {
    let n = ssm.len();
    let mut out = vec![0.0; n];
    if n < lags * 2 {
        return out;
    }
    let k = checkerboard(lags);
    for (c, o) in out.iter_mut().enumerate().take(n - lags).skip(lags) {
        let mut sum = 0.0;
        for a in 0..2 * lags {
            let row = &ssm[c + a - lags];
            for b in 0..2 * lags {
                sum += k[a][b] * row[c + b - lags];
            }
        }
        *o = sum.max(0.0);
    }
    out
}

/// Section boundaries (including `0` and `duration_ms`) at novelty peaks,
/// keeping sections at least [`MIN_SEGMENT_MS`] long.
pub fn novelty_boundaries(novelty: &[f64], ms_per_step: i64, duration_ms: i64) -> Vec<i64> {
    let mut bounds = vec![0];
    let positive: Vec<f64> = novelty.iter().copied().filter(|&v| v > 0.0).collect();
    if positive.is_empty() || ms_per_step <= 0 {
        bounds.push(duration_ms);
        return bounds;
    }
    let threshold = mean(&positive) * NOVELTY_THRESHOLD;
    for i in 1..novelty.len().saturating_sub(1) {
        if novelty[i] < threshold || novelty[i] < novelty[i - 1] || novelty[i] < novelty[i + 1] {
            continue;
        }
        let ms = i as i64 * ms_per_step;
        if ms - bounds[bounds.len() - 1] >= MIN_SEGMENT_MS && duration_ms - ms >= MIN_SEGMENT_MS {
            bounds.push(ms);
        }
    }
    bounds.push(duration_ms);
    bounds
}

#[derive(Clone, Copy)]
struct Span {
    from: usize,
    to: usize,
}

fn span_similarity(ssm: &[Vec<f64>], a: Span, b: Span) -> f64 {
    let length = (a.to - a.from).min(b.to - b.from);
    if length == 0 {
        return 0.0;
    }
    let (mut sum, mut count) = (0.0, 0);
    for o in 0..=length {
        let (x, y) = (a.from + o, b.from + o);
        if x >= ssm.len() || y >= ssm.len() {
            break;
        }
        sum += ssm[x][y];
        count += 1;
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

fn repetition(ssm: &[Vec<f64>], spans: &[Span]) -> Vec<f64> {
    (0..spans.len())
        .map(|i| {
            if spans.len() <= 1 {
                return 0.0;
            }
            let repeats = (0..spans.len())
                .filter(|&j| i != j && span_similarity(ssm, spans[i], spans[j]) >= REPEAT_THRESHOLD)
                .count();
            repeats as f64 / (spans.len() - 1) as f64
        })
        .collect()
}

fn smooth(values: &[f64]) -> Vec<f64> {
    (0..values.len())
        .map(|i| {
            let lo = i.saturating_sub(SMOOTH_RADIUS);
            let hi = (values.len() - 1).min(i + SMOOTH_RADIUS);
            mean(&values[lo..=hi])
        })
        .collect()
}

fn energy_boundaries(smoothed: &[f64], energy: &[EnergyPoint], duration_ms: i64) -> Vec<i64> {
    let mut novelty = vec![0.0; smoothed.len()];
    for i in 1..smoothed.len() {
        novelty[i] = (smoothed[i] - smoothed[i - 1]).abs();
    }
    let threshold = mean(&novelty) * 2.5;
    let mut bounds = vec![0];
    for i in 1..novelty.len() {
        if novelty[i] > threshold {
            let ms = energy[i].time_ms;
            if ms - bounds[bounds.len() - 1] >= MIN_SEGMENT_MS && duration_ms - ms >= MIN_SEGMENT_MS {
                bounds.push(ms);
            }
        }
    }
    bounds.push(duration_ms);
    bounds
}

fn classify(index: usize, total: usize, level: f64, slope: f64, mean_level: f64, repeat: f64) -> &'static str {
    let quiet = level <= mean_level * EDGE_ENERGY_RATIO;
    if index == 0 && quiet {
        "intro"
    } else if index == total - 1 && quiet {
        "outro"
    } else if slope > BUILD_SLOPE {
        "build"
    } else if repeat >= CHORUS_REPEAT && level >= LOUD_LEVEL {
        "chorus"
    } else if repeat >= CHORUS_REPEAT {
        "verse"
    } else if level <= QUIET_LEVEL {
        "breakdown"
    } else if level >= LOUD_LEVEL {
        "drop"
    } else if repeat > 0.0 {
        "verse"
    } else {
        "bridge"
    }
}

fn beat_index_at(beats: &[i64], ms: i64) -> i64 {
    if beats.is_empty() {
        return 0;
    }
    let i = beats.partition_point(|&b| b < ms);
    i.min(beats.len() - 1) as i64
}

/// Split the track into labelled sections.
///
/// Uses novelty over the self-similarity of `features` (one vector every
/// `ms_per_feature`) when there are enough of them, otherwise jumps in the
/// smoothed energy curve.
pub fn segment(
    energy: &[EnergyPoint],
    beats: &[i64],
    duration_ms: i64,
    features: &[Vec<f64>],
    ms_per_feature: i64,
) -> Vec<Section> {
    if energy.is_empty() || duration_ms <= 0 {
        return Vec::new();
    }
    let raw: Vec<f64> = energy.iter().map(|p| p.value).collect();
    let smoothed = smooth(&raw);
    let peak = max_of(&smoothed).max(1e-6);
    let mean_level = (mean(&smoothed) / peak).max(1e-6);

    let ssm = (features.len() >= KERNEL_LAGS * 2 && ms_per_feature > 0).then(|| similarity_matrix(features));
    let bounds = match &ssm {
        Some(ssm) => novelty_boundaries(&novelty(ssm, KERNEL_LAGS), ms_per_feature, duration_ms),
        None => energy_boundaries(&smoothed, energy, duration_ms),
    };
    let sections_n = bounds.len() - 1;
    let repeats = match &ssm {
        Some(ssm) => {
            let last = ssm.len() as i64 - 1;
            let spans: Vec<Span> = bounds
                .windows(2)
                .map(|w| {
                    let from = clamp_int(w[0] / ms_per_feature, 0, last);
                    let to = clamp_int(w[1] / ms_per_feature, from, last);
                    Span {
                        from: from as usize,
                        to: to as usize,
                    }
                })
                .collect();
            repetition(ssm, &spans)
        }
        None => vec![0.0; sections_n],
    };
    let mut sections = Vec::with_capacity(sections_n);
    for (i, w) in bounds.windows(2).enumerate() {
        let (start, end) = (w[0], w[1]);
        let (mut sum, mut count, mut first_idx, mut last_idx) = (0.0, 0, None, None);
        for (j, p) in energy.iter().enumerate() {
            if p.time_ms >= start && p.time_ms < end {
                sum += smoothed[j];
                count += 1;
                first_idx.get_or_insert(j);
                last_idx = Some(j);
            }
        }
        let level = if count > 0 { sum / count as f64 / peak } else { 0.0 };
        let slope = match (first_idx, last_idx) {
            (Some(f), Some(l)) if l > f => smoothed[l] - smoothed[f],
            _ => 0.0,
        };
        sections.push(Section {
            kind: classify(i, sections_n, level, slope, mean_level, repeats[i]).into(),
            start_ms: start,
            end_ms: end,
            energy: clamp(level, 0.0, 1.0),
            beat_start: beat_index_at(beats, start),
            beat_end: beat_index_at(beats, end),
        });
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::loud_frames;

    #[test]
    fn loudness_six_decibel_step() {
        let measure = |amp: f64| {
            let mut l = Loudness::new();
            for f in loud_frames(amp, 3) {
                l.feed(&f);
            }
            l.integrated()
        };
        let diff = measure(0.5) - measure(0.25);
        assert!((diff - 6.02).abs() <= 0.1, "step {diff:.3} dB");
        assert_eq!(Loudness::new().integrated(), FLOOR_LUFS, "empty loudness");
    }

    #[test]
    fn loudness_of_full_scale_sine_is_near_zero() {
        // A full-scale 1 kHz sine reads -3.01 LUFS per channel; on both
        // channels BS.1770 sums them to about 0 LUFS.
        let mut l = Loudness::new();
        for f in loud_frames(1.0, 3) {
            l.feed(&f);
        }
        assert!(l.integrated().abs() < 0.5, "{}", l.integrated());
        assert!((l.peak() - 1.0).abs() < 1e-3);
    }

    fn curve(values: &[f64]) -> Vec<EnergyPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, &v)| EnergyPoint {
                time_ms: i as i64 * 500,
                value: v,
            })
            .collect()
    }

    #[test]
    fn mix_regions_find_intro_and_outro() {
        // 10 s quiet intro, 40 s full, 10 s fade, 60 s total.
        let mut v = vec![0.3; 20];
        v.extend(vec![1.0; 80]);
        v.extend(vec![0.3; 20]);
        let energy = curve(&v);
        let phrases: Vec<i64> = (0..8).map(|i| i * 8000).collect();
        let m = detect_mix_regions(&energy, &[], &phrases, 60_000);
        assert_eq!(m.intro_start_ms, 0);
        assert_eq!(m.intro_end_ms, 8000, "{m:?}");
        assert_eq!(m.outro_start_ms, 48000, "{m:?}");
        assert_eq!(m.outro_end_ms, 59_500);
        let none = detect_mix_regions(&[], &[], &[], 1000);
        assert_eq!((none.outro_start_ms, none.outro_end_ms), (1000, 1000));
    }

    #[test]
    fn segment_by_energy_labels_intro_and_drop() {
        let mut v = vec![0.2; 40];
        v.extend(vec![1.0; 80]);
        v.extend(vec![0.2; 40]);
        let energy = curve(&v);
        let sections = segment(&energy, &[], 80_000, &[], 0);
        assert!(sections.len() >= 3, "{sections:?}");
        assert_eq!(sections[0].kind, "intro");
        assert_eq!(sections.last().unwrap().kind, "outro");
        assert!(
            sections.iter().any(|s| s.kind == "drop" || s.kind == "build"),
            "{sections:?}"
        );
        assert_eq!(sections.last().unwrap().end_ms, 80_000);
    }

    #[test]
    fn novelty_splits_two_timbres() {
        let mut features = vec![vec![1.0, 0.0, 0.0]; 80];
        features.extend(vec![vec![0.0, 1.0, 0.0]; 80]);
        let n = novelty(&similarity_matrix(&features), KERNEL_LAGS);
        let peak = (0..n.len()).max_by(|&a, &b| n[a].total_cmp(&n[b])).unwrap();
        assert!((78..=82).contains(&peak), "peak at {peak}");
        let bounds = novelty_boundaries(&n, 500, 80_000);
        assert_eq!(bounds.len(), 3, "{bounds:?}");
        assert!((bounds[1] - 40_000).abs() <= 1000, "{bounds:?}");
    }
}
