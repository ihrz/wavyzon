//! Signal generators, a WAV builder, level/frequency measurements and
//! tolerance assertions.
//!
//! Samples are interleaved `i16`, like everything in `wavyzon-audio`. Levels
//! are normalised to full scale (`1.0` = 32768).
//!
//! ```
//! use wavyzon_testkit::audio::*;
//!
//! let pcm = sine(440.0, 500, 0.5, 48_000, 2);
//! let left = channel(&pcm, 2, 0);
//! assert_frequency(&left, 48_000, 440.0, 2.0);
//! assert_within(rms(&pcm), 0.5 / 2f64.sqrt(), 0.01, "rms");
//!
//! let wav = WavBuilder::new(48_000, 2).tone(440.0, 200, 0.5).silence(100).build();
//! assert_eq!(&wav[..4], b"RIFF");
//! ```

use std::f64::consts::PI;
use std::io::Cursor;

use tokio::io::AsyncReadExt;
use wavyzon_audio::Frame;
use wavyzon_audio::codec::{self, CodecError};
use wavyzon_plugin::{Format, Stream};

/// Full scale of an `i16` sample.
pub const FULL_SCALE: f64 = 32768.0;

fn count(ms: u64, rate: u32) -> usize {
    (ms as u128 * rate as u128 / 1000) as usize
}

fn to_i16(v: f64) -> i16 {
    (v * FULL_SCALE).round().clamp(-32768.0, 32767.0) as i16
}

/// One sample of a sine of `freq` Hz at absolute sample index `n`.
pub fn sine_sample(freq: f64, amplitude: f64, rate: u32, n: u64) -> i16 {
    to_i16(amplitude * (2.0 * PI * freq * n as f64 / rate as f64).sin())
}

/// `ms` of a sine at `freq` Hz, `amplitude` in `0.0..=1.0`, same on every channel.
pub fn sine(freq: f64, ms: u64, amplitude: f64, rate: u32, channels: usize) -> Vec<i16> {
    let n = count(ms, rate);
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let s = sine_sample(freq, amplitude, rate, i as u64);
        out.extend(std::iter::repeat_n(s, channels));
    }
    out
}

/// `ms` of digital silence.
pub fn silence(ms: u64, rate: u32, channels: usize) -> Vec<i16> {
    vec![0; count(ms, rate) * channels]
}

/// A click track: a 1 ms square burst of `amplitude` on every beat at `bpm`,
/// the first one at t = 0, silence between.
pub fn clicks(bpm: f64, ms: u64, amplitude: f64, rate: u32, channels: usize) -> Vec<i16> {
    let n = count(ms, rate);
    let period = rate as f64 * 60.0 / bpm;
    let width = (rate as usize / 1000).max(1);
    let mut out = vec![0i16; n * channels];
    let mut beat = 0.0f64;
    while (beat as usize) < n {
        let start = beat.round() as usize;
        for i in start..(start + width).min(n) {
            let v = if (i - start) % 2 == 0 { amplitude } else { -amplitude };
            out[i * channels..(i + 1) * channels].fill(to_i16(v));
        }
        beat += period;
    }
    out
}

/// Deterministic white noise (xorshift) of `amplitude`.
pub fn noise(ms: u64, amplitude: f64, rate: u32, channels: usize, seed: u64) -> Vec<i16> {
    let mut x = seed | 1;
    (0..count(ms, rate) * channels)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            to_i16(amplitude * ((x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0))
        })
        .collect()
}

/// Encode interleaved `samples` as a 16-bit PCM RIFF/WAVE file.
pub fn wav_bytes(samples: &[i16], rate: u32, channels: usize) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let block = (channels * 2) as u16;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&(channels as u16).to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block as u32).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Builds a WAV file out of segments.
///
/// ```
/// use wavyzon_testkit::audio::WavBuilder;
/// let wav = WavBuilder::new(44_100, 2).silence(50).clicks(120.0, 2000, 0.8).tone(1000.0, 250, 0.3).build();
/// # assert!(wav.len() > 44);
/// ```
#[derive(Debug, Clone)]
pub struct WavBuilder {
    rate: u32,
    channels: usize,
    samples: Vec<i16>,
}

impl WavBuilder {
    /// An empty file at `rate` Hz with `channels` channels.
    pub fn new(rate: u32, channels: usize) -> Self {
        Self {
            rate,
            channels,
            samples: Vec::new(),
        }
    }

    /// Append a sine segment.
    pub fn tone(mut self, freq: f64, ms: u64, amplitude: f64) -> Self {
        self.samples.extend(sine(freq, ms, amplitude, self.rate, self.channels));
        self
    }

    /// Append silence.
    pub fn silence(mut self, ms: u64) -> Self {
        self.samples.extend(silence(ms, self.rate, self.channels));
        self
    }

    /// Append a click track segment.
    pub fn clicks(mut self, bpm: f64, ms: u64, amplitude: f64) -> Self {
        self.samples
            .extend(clicks(bpm, ms, amplitude, self.rate, self.channels));
        self
    }

    /// Append noise.
    pub fn noise(mut self, ms: u64, amplitude: f64, seed: u64) -> Self {
        self.samples
            .extend(noise(ms, amplitude, self.rate, self.channels, seed));
        self
    }

    /// Append raw interleaved samples.
    pub fn samples(mut self, samples: &[i16]) -> Self {
        self.samples.extend_from_slice(samples);
        self
    }

    /// Duration so far, in ms.
    pub fn duration_ms(&self) -> u64 {
        (self.samples.len() / self.channels) as u64 * 1000 / self.rate as u64
    }

    /// The samples so far.
    pub fn pcm(&self) -> &[i16] {
        &self.samples
    }

    /// Encode the file.
    pub fn build(&self) -> Vec<u8> {
        wav_bytes(&self.samples, self.rate, self.channels)
    }
}

// ----- conversions -----------------------------------------------------------

/// Extract channel `index` of interleaved samples.
pub fn channel(samples: &[i16], channels: usize, index: usize) -> Vec<i16> {
    samples.chunks_exact(channels).map(|c| c[index]).collect()
}

/// Average all channels into mono.
pub fn mono(samples: &[i16], channels: usize) -> Vec<i16> {
    samples
        .chunks_exact(channels)
        .map(|c| (c.iter().map(|s| *s as i32).sum::<i32>() / channels as i32) as i16)
        .collect()
}

/// Concatenate frames into one interleaved buffer.
pub fn frames_to_samples(frames: &[Frame]) -> Vec<i16> {
    frames.iter().flat_map(|f| f.iter().copied()).collect()
}

/// Little-endian `i16` bytes (the `Format::Pcm48` wire format) to samples.
pub fn pcm_from_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Samples to little-endian bytes (`Format::Pcm48`).
pub fn pcm_to_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Read up to `max_bytes` of a stream's body.
pub async fn read_body(stream: &mut Stream, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut stream.body).take(max_bytes as u64).read_to_end(&mut out).await?;
    Ok(out)
}

/// Read up to `max_ms` of a `Format::Pcm48` stream as samples.
pub async fn read_pcm48(stream: &mut Stream, max_ms: u64) -> std::io::Result<Vec<i16>> {
    let bytes = read_body(stream, count(max_ms, 48_000) * 4).await?;
    Ok(pcm_from_bytes(&bytes))
}

/// Decode an in-memory file with `wavyzon_audio::codec::decode`; returns the
/// interleaved stereo samples and the native sample rate. Blocking.
pub fn decode_bytes(format: &Format, bytes: Vec<u8>) -> Result<(Vec<i16>, u32), CodecError> {
    let mut pcm = codec::decode(format, Box::new(Cursor::new(bytes)))?;
    let mut out = Vec::new();
    let mut buf = vec![0i16; 4096];
    loop {
        let n = pcm.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok((out, pcm.sample_rate()))
}

// ----- measurements ----------------------------------------------------------

/// Root mean square level, `0.0..=1.0`.
pub fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64).powi(2)).sum();
    (sum / samples.len() as f64).sqrt() / FULL_SCALE
}

/// Peak absolute level, `0.0..=1.0`.
pub fn peak(samples: &[i16]) -> f64 {
    samples.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max) / FULL_SCALE
}

/// A normalised level in dBFS (`-inf` for 0).
pub fn dbfs(level: f64) -> f64 {
    20.0 * level.log10()
}

/// Frequency estimated from (rising + falling) zero crossings of a mono
/// signal. Good for clean tones; useless for noise or chords.
pub fn zero_crossing_frequency(mono: &[i16], rate: u32) -> f64 {
    let mut crossings = Vec::new();
    for i in 1..mono.len() {
        let (a, b) = (mono[i - 1] as f64, mono[i] as f64);
        if (a < 0.0 && b >= 0.0) || (a >= 0.0 && b < 0.0) {
            // Linear interpolation of the crossing instant.
            crossings.push((i - 1) as f64 + a / (a - b));
        }
    }
    if crossings.len() < 2 {
        return 0.0;
    }
    let span = crossings[crossings.len() - 1] - crossings[0];
    (crossings.len() - 1) as f64 / 2.0 * rate as f64 / span
}

/// Amplitude (`0.0..=1.0`) of the `freq` Hz component of a mono signal,
/// via the Goertzel algorithm. A pure sine of amplitude `a` at an exact bin
/// gives ≈ `a`.
pub fn goertzel(mono: &[i16], rate: u32, freq: f64) -> f64 {
    if mono.is_empty() {
        return 0.0;
    }
    let w = 2.0 * PI * freq / rate as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in mono {
        let s = x as f64 / FULL_SCALE + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    2.0 * power.sqrt() / mono.len() as f64
}

/// The candidate in `lo..=hi` (step `step` Hz) with the largest Goertzel
/// amplitude.
pub fn dominant_frequency(mono: &[i16], rate: u32, lo: f64, hi: f64, step: f64) -> f64 {
    let mut best = (lo, f64::MIN);
    let mut f = lo;
    while f <= hi {
        let a = goertzel(mono, rate, f);
        if a > best.1 {
            best = (f, a);
        }
        f += step;
    }
    best.0
}

/// Onset times (ms) where `|x|` rises above `threshold` (`0.0..=1.0`) after
/// at least `min_gap_ms` below it. Finds clicks of [`clicks`].
pub fn onsets(mono: &[i16], rate: u32, threshold: f64, min_gap_ms: f64) -> Vec<f64> {
    let thr = threshold * FULL_SCALE;
    let gap = min_gap_ms * rate as f64 / 1000.0;
    let mut out = Vec::new();
    let mut last_loud: Option<usize> = None;
    for (i, &s) in mono.iter().enumerate() {
        if (s as f64).abs() >= thr {
            if last_loud.is_none_or(|l| (i - l) as f64 > gap) {
                out.push(i as f64 * 1000.0 / rate as f64);
            }
            last_loud = Some(i);
        }
    }
    out
}

/// Tempo from onset times: 60000 / median inter-onset interval.
pub fn estimate_bpm(onsets_ms: &[f64]) -> Option<f64> {
    if onsets_ms.len() < 2 {
        return None;
    }
    let mut d: Vec<f64> = onsets_ms.windows(2).map(|w| w[1] - w[0]).collect();
    d.sort_by(f64::total_cmp);
    Some(60_000.0 / d[d.len() / 2])
}

// ----- assertions ------------------------------------------------------------

/// `|actual - expected| <= tolerance`.
pub fn within(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual - expected).abs() <= tolerance
}

/// Panic unless `|actual - expected| <= tolerance`.
#[track_caller]
pub fn assert_within(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        within(actual, expected, tolerance),
        "{what}: got {actual}, expected {expected} ± {tolerance} (off by {})",
        (actual - expected).abs()
    );
}

/// Panic unless a position (ms) is within `tolerance_ms` of `expected_ms`.
#[track_caller]
pub fn assert_position_ms(actual_ms: i64, expected_ms: i64, tolerance_ms: i64) {
    assert!(
        (actual_ms - expected_ms).abs() <= tolerance_ms,
        "position {actual_ms} ms, expected {expected_ms} ± {tolerance_ms} ms"
    );
}

/// Panic unless the mono signal is a tone at `expected` Hz (± `tolerance_hz`
/// by zero crossings) whose Goertzel energy at `expected` dominates.
#[track_caller]
pub fn assert_frequency(mono: &[i16], rate: u32, expected: f64, tolerance_hz: f64) {
    let zc = zero_crossing_frequency(mono, rate);
    assert!(
        within(zc, expected, tolerance_hz),
        "zero-crossing frequency {zc:.2} Hz, expected {expected} ± {tolerance_hz} Hz"
    );
    let at = goertzel(mono, rate, expected);
    for off in [expected * 0.5, expected * 1.5, expected * 2.0] {
        let other = goertzel(mono, rate, off);
        assert!(
            at > other * 4.0,
            "Goertzel at {expected} Hz = {at:.4} does not dominate {off} Hz = {other:.4}"
        );
    }
}

/// Panic unless RMS `<= max_rms`.
#[track_caller]
pub fn assert_silent(samples: &[i16], max_rms: f64) {
    let r = rms(samples);
    assert!(r <= max_rms, "expected silence (rms <= {max_rms}), got rms {r:.4}");
}

/// Panic unless RMS `>= min_rms`.
#[track_caller]
pub fn assert_audible(samples: &[i16], min_rms: f64) {
    let r = rms(samples);
    assert!(r >= min_rms, "expected audio (rms >= {min_rms}), got rms {r:.4}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_levels_and_frequency() {
        let s = sine(1000.0, 1000, 0.5, 48_000, 1);
        assert_eq!(s.len(), 48_000);
        assert_within(rms(&s), 0.5 / 2f64.sqrt(), 0.001, "rms");
        assert_within(peak(&s), 0.5, 0.001, "peak");
        assert_within(zero_crossing_frequency(&s, 48_000), 1000.0, 0.5, "zc");
        assert_within(goertzel(&s, 48_000, 1000.0), 0.5, 0.01, "goertzel");
        assert!(goertzel(&s, 48_000, 1500.0) < 0.01);
        assert_eq!(dominant_frequency(&s, 48_000, 100.0, 2000.0, 10.0), 1000.0);
        assert_frequency(&s, 48_000, 1000.0, 1.0);
    }

    #[test]
    fn silence_and_noise() {
        assert_silent(&silence(100, 48_000, 2), 0.0);
        let n = noise(200, 0.5, 48_000, 2, 42);
        assert_audible(&n, 0.2);
        assert_eq!(n, noise(200, 0.5, 48_000, 2, 42));
        assert!(peak(&n) <= 0.5 + 1e-4);
        assert_eq!(dbfs(1.0), 0.0);
    }

    #[test]
    fn clicks_give_bpm() {
        let c = clicks(120.0, 4000, 0.9, 48_000, 2);
        let on = onsets(&channel(&c, 2, 0), 48_000, 0.5, 50.0);
        assert_eq!(on.len(), 8);
        assert_within(on[1], 500.0, 0.1, "second click");
        assert_within(estimate_bpm(&on).unwrap(), 120.0, 0.1, "bpm");
    }

    #[test]
    fn wav_header() {
        let w = WavBuilder::new(48_000, 2).tone(440.0, 100, 0.5);
        assert_eq!(w.duration_ms(), 100);
        let b = w.build();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(b.len(), 44 + 4800 * 4);
        assert_eq!(pcm_from_bytes(&b[44..]), w.pcm());
        assert_eq!(pcm_to_bytes(w.pcm()), b[44..].to_vec());
    }

    #[test]
    fn channels_and_mono() {
        let s = [1i16, 3, 5, 7];
        assert_eq!(channel(&s, 2, 1), vec![3, 7]);
        assert_eq!(mono(&s, 2), vec![2, 6]);
        assert_position_ms(1010, 1000, 20);
    }

    #[test]
    #[should_panic(expected = "position")]
    fn position_assert_panics() {
        assert_position_ms(1100, 1000, 20);
    }
}
