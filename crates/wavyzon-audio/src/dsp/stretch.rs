//! WSOLA time stretcher: changes tempo without changing pitch, and carries a
//! source position with every output hop.

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::FRAME_SAMPLES_PER_CHANNEL;

/// Output hop, in samples per channel (one 20 ms frame).
pub const STRETCH_HOP: usize = FRAME_SAMPLES_PER_CHANNEL;
const STRETCH_WINDOW: usize = STRETCH_HOP * 2;
const STRETCH_SEARCH: usize = 480;
const SEARCH_STRIDE: usize = 4;

fn hann() -> &'static [f64] {
    static HANN: OnceLock<Vec<f64>> = OnceLock::new();
    HANN.get_or_init(|| {
        (0..STRETCH_WINDOW)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / STRETCH_WINDOW as f64).cos())
            .collect()
    })
}

/// Waveform-similarity overlap-add stretcher over interleaved stereo `f64`.
///
/// Each input sample comes with a source position (whatever unit the caller
/// likes); [`next`](Self::next) returns the position of the first input
/// sample used by the hop, so positions stay exact at any tempo.
#[derive(Debug, Clone)]
pub struct Stretcher {
    tempo: f64,
    input: Vec<f64>,
    pos: Vec<f64>,
    nominal: f64,
    prev_start: usize,
    tail: Vec<f64>,
    started: bool,
}

impl Default for Stretcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Stretcher {
    /// A stretcher at tempo `1.0` (exact passthrough until the tempo changes).
    pub fn new() -> Self {
        Self {
            tempo: 1.0,
            input: Vec::new(),
            pos: Vec::new(),
            nominal: 0.0,
            prev_start: 0,
            tail: vec![0.0; STRETCH_HOP * 2],
            started: false,
        }
    }

    /// Set the tempo, clamped to `0.25..=4`. Non-positive values mean `1`.
    pub fn set_tempo(&mut self, tempo: f64) {
        let tempo = if tempo <= 0.0 { 1.0 } else { tempo };
        self.tempo = tempo.clamp(0.25, 4.0);
    }

    /// Current tempo.
    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    fn frames(&self) -> usize {
        self.pos.len()
    }

    /// Append interleaved `samples` and one position per stereo sample.
    pub fn write(&mut self, samples: &[f64], positions: &[f64]) {
        self.input.extend_from_slice(samples);
        self.pos.extend_from_slice(positions);
    }

    /// Stereo samples buffered beyond the read head.
    pub fn buffered(&self) -> isize {
        self.frames() as isize - self.nominal as isize
    }

    /// Drop all buffered input and overlap state.
    pub fn reset(&mut self) {
        self.input.clear();
        self.pos.clear();
        self.nominal = 0.0;
        self.prev_start = 0;
        self.started = false;
        self.tail.fill(0.0);
    }

    /// Produce one hop ([`STRETCH_HOP`] stereo samples) into `out`. Returns the
    /// source position of the hop, or `None` when more input is needed.
    pub fn next(&mut self, out: &mut [f64]) -> Option<f64> {
        if self.tempo == 1.0 && !self.started {
            return self.passthrough(out);
        }
        let mut start = self.nominal as usize;
        if start + STRETCH_SEARCH + STRETCH_WINDOW >= self.frames() {
            return None;
        }
        if self.started {
            start = self.best_offset(start);
        } else {
            self.started = true;
        }
        let hann = hann();
        for i in 0..STRETCH_HOP {
            let (w1, w2) = (hann[i], hann[i + STRETCH_HOP]);
            for c in 0..2 {
                out[2 * i + c] = self.tail[2 * i + c] + self.input[2 * (start + i) + c] * w1;
                self.tail[2 * i + c] = self.input[2 * (start + STRETCH_HOP + i) + c] * w2;
            }
        }
        let position = self.pos[start];
        self.prev_start = start;
        self.nominal += STRETCH_HOP as f64 * self.tempo;
        self.compact();
        Some(position)
    }

    fn passthrough(&mut self, out: &mut [f64]) -> Option<f64> {
        let start = self.nominal as usize;
        if start + STRETCH_HOP > self.frames() {
            return None;
        }
        out[..2 * STRETCH_HOP].copy_from_slice(&self.input[2 * start..2 * (start + STRETCH_HOP)]);
        let position = self.pos[start];
        self.prev_start = start;
        self.nominal += STRETCH_HOP as f64;
        self.compact();
        Some(position)
    }

    fn best_offset(&self, nominal: usize) -> usize {
        let natural = self.prev_start + STRETCH_HOP;
        let (mut best, mut best_score) = (nominal, f64::NEG_INFINITY);
        let lo = nominal.saturating_sub(STRETCH_SEARCH);
        let mut cand = lo;
        while cand <= nominal + STRETCH_SEARCH {
            let mut score = 0.0;
            let mut i = 0;
            while i < STRETCH_HOP {
                let a = self.input[2 * (natural + i)] + self.input[2 * (natural + i) + 1];
                let b = self.input[2 * (cand + i)] + self.input[2 * (cand + i) + 1];
                score += a * b;
                i += SEARCH_STRIDE;
            }
            if score > best_score {
                best = cand;
                best_score = score;
            }
            cand += SEARCH_STRIDE;
        }
        best
    }

    fn compact(&mut self) {
        let keep = (self.nominal as isize).min(self.prev_start as isize) - STRETCH_SEARCH as isize;
        if keep < 8192 {
            return;
        }
        let keep = keep as usize;
        self.input.drain(..2 * keep);
        self.pos.drain(..keep);
        self.nominal -= keep as f64;
        self.prev_start -= keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;
    use crate::dsp::test_util::sine;

    #[test]
    fn tracks_source_position() {
        let mut s = Stretcher::new();
        s.set_tempo(1.25);
        let src = sine(SAMPLE_RATE as f64, 440.0, 4.0);
        let samples: Vec<f64> = src.iter().map(|&v| v as f64).collect();
        let positions: Vec<f64> = (0..src.len() / 2).map(|i| i as f64).collect();
        s.write(&samples, &positions);
        let mut out = vec![0.0; 2 * STRETCH_HOP];
        let mut last = 0.0;
        let mut hops = 0;
        while let Some(p) = s.next(&mut out) {
            last = p;
            hops += 1;
        }
        let elapsed = (hops * STRETCH_HOP) as f64;
        let ratio = last / elapsed;
        assert!(
            (ratio - 1.25).abs() <= 0.05,
            "source advanced {ratio:.3}x of output, want 1.25"
        );
    }

    #[test]
    fn passthrough_is_exact() {
        let mut s = Stretcher::new();
        let mut big = vec![0.0; 2 * STRETCH_HOP];
        big[..4].copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
        s.write(&big, &vec![0.0; STRETCH_HOP]);
        let mut out = vec![0.0; 2 * STRETCH_HOP];
        assert!(s.next(&mut out).is_some());
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
    }
}
