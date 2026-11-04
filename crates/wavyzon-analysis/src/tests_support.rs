//! Synthetic signals for the tests.

use std::f64::consts::PI;

use wavyzon_audio::{FRAME_SAMPLES, FRAME_SAMPLES_PER_CHANNEL, FRAMES_PER_SECOND, Frame, SAMPLE_RATE, clip};

fn to_frames(samples: &[i16]) -> Vec<Frame> {
    samples
        .chunks_exact(FRAME_SAMPLES)
        .map(|c| {
            let mut f = [0i16; FRAME_SAMPLES];
            f.copy_from_slice(c);
            f
        })
        .collect()
}

/// `seconds` of a 1 kHz sine at `amplitude` (0..=1) on both channels.
pub fn loud_frames(amplitude: f64, seconds: i64) -> Vec<Frame> {
    let n = (seconds * FRAMES_PER_SECOND) as usize * FRAME_SAMPLES_PER_CHANNEL;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = clip(amplitude * 32767.0 * (2.0 * PI * 1000.0 * i as f64 / SAMPLE_RATE as f64).sin());
        out.extend([v, v]);
    }
    to_frames(&out)
}

/// `seconds` of equal-amplitude sines at `freqs`, both channels.
pub fn tone_frames(freqs: &[f64], seconds: i64) -> Vec<Frame> {
    let n = (seconds * FRAMES_PER_SECOND) as usize * FRAME_SAMPLES_PER_CHANNEL;
    let amp = 0.6 / freqs.len() as f64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / SAMPLE_RATE as f64;
        let v = clip(32767.0 * amp * freqs.iter().map(|f| (2.0 * PI * f * t).sin()).sum::<f64>());
        out.extend([v, v]);
    }
    to_frames(&out)
}

/// Interleaved stereo clicks at `bpm`, every `bar`-th one accented, at `rate`.
pub fn click_samples(rate: u32, bpm: f64, bar: usize, seconds: usize) -> Vec<i16> {
    let n = rate as usize * seconds;
    let period = 60.0 / bpm * rate as f64;
    let click_len = (rate as f64 * 0.01) as usize;
    let mut out = vec![0i16; n * 2];
    let mut beat = 0usize;
    let mut pos = 0.0;
    while (pos as usize) < n {
        let start = pos as usize;
        let amp = if beat % bar == 0 { 0.9 } else { 0.35 };
        for k in 0..click_len.min(n - start) {
            let env = (-(k as f64) / (click_len as f64 / 5.0)).exp();
            // Broadband click: a few harmonics of 1.5 kHz plus a low thump.
            let t = k as f64 / rate as f64;
            let s = (2.0 * PI * 1500.0 * t).sin() + 0.5 * (2.0 * PI * 3100.0 * t).sin() + (2.0 * PI * 120.0 * t).sin();
            let v = clip(32767.0 * amp * 0.4 * env * s);
            out[2 * (start + k)] = v;
            out[2 * (start + k) + 1] = v;
        }
        beat += 1;
        pos += period;
    }
    out
}

/// 48 kHz click frames, see [`click_samples`].
pub fn click_frames(bpm: f64, bar: usize, seconds: usize) -> Vec<Frame> {
    to_frames(&click_samples(SAMPLE_RATE, bpm, bar, seconds))
}

/// A 16-bit stereo PCM WAV file.
pub fn wav_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
