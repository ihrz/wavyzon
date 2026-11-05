//! Uniformly partitioned FFT convolution (overlap-save) with one partition
//! per 20 ms frame, so a whole frame is convolved as it arrives: no added
//! latency, and the wire clock stays exact.

use std::sync::{Arc, OnceLock};

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Samples per channel in one frame, and the partition length.
pub const BLOCK: usize = 960;
const FFT: usize = 2 * BLOCK;
const BINS: usize = BLOCK + 1;

struct Plans {
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
}

fn plans() -> &'static Plans {
    static PLANS: OnceLock<Plans> = OnceLock::new();
    PLANS.get_or_init(|| {
        let mut p = RealFftPlanner::<f32>::new();
        Plans {
            fwd: p.plan_fft_forward(FFT),
            inv: p.plan_fft_inverse(FFT),
        }
    })
}

/// An impulse response cut into partitions and transformed once.
#[derive(Debug)]
pub struct Kernel {
    parts: Vec<Vec<Complex<f32>>>,
    taps: usize,
}

impl Kernel {
    /// Transform `taps` (at 48 kHz). The 1/N of the inverse FFT is folded in.
    pub fn new(taps: &[f32]) -> Self {
        let p = plans();
        let mut input = p.fwd.make_input_vec();
        let mut scratch = p.fwd.make_scratch_vec();
        let scale = 1.0 / FFT as f32;
        let parts = taps
            .chunks(BLOCK)
            .map(|chunk| {
                input.fill(0.0);
                for (d, s) in input.iter_mut().zip(chunk) {
                    *d = s * scale;
                }
                let mut out = p.fwd.make_output_vec();
                // Sizes come from the plan itself; this cannot fail.
                let _ = p.fwd.process_with_scratch(&mut input, &mut out, &mut scratch);
                out
            })
            .collect::<Vec<_>>();
        Self {
            parts: if parts.is_empty() {
                vec![vec![Complex::default(); BINS]]
            } else {
                parts
            },
            taps: taps.len(),
        }
    }

    pub fn taps(&self) -> usize {
        self.taps
    }

    pub fn partitions(&self) -> usize {
        self.parts.len()
    }
}

/// Buffers every convolver of a filter instance can share (one runs at a
/// time). Allocated once, when the instance is built.
pub struct Scratch {
    time: Vec<f32>,
    out: Vec<f32>,
    fwd: Vec<Complex<f32>>,
    inv: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
}

impl Default for Scratch {
    fn default() -> Self {
        let p = plans();
        Self {
            time: vec![0.0; FFT],
            out: vec![0.0; FFT],
            fwd: p.fwd.make_scratch_vec(),
            inv: p.inv.make_scratch_vec(),
            acc: vec![Complex::default(); BINS],
        }
    }
}

/// One channel's running convolution. [`process`](Self::process) does not
/// allocate.
pub struct Convolver {
    kernel: Arc<Kernel>,
    /// Spectra of the last `partitions` input windows, newest at `head`.
    fdl: Vec<Vec<Complex<f32>>>,
    head: usize,
    /// Previous block followed by the current one.
    history: Vec<f32>,
}

impl Convolver {
    pub fn new(kernel: Arc<Kernel>) -> Self {
        let n = kernel.partitions();
        Self {
            kernel,
            fdl: vec![vec![Complex::default(); BINS]; n],
            head: 0,
            history: vec![0.0; FFT],
        }
    }

    /// Convolve one block of [`BLOCK`] samples in place.
    pub fn process(&mut self, block: &mut [f32], s: &mut Scratch) {
        debug_assert_eq!(block.len(), BLOCK);
        let p = plans();
        let n = self.fdl.len();
        self.history.copy_within(BLOCK.., 0);
        self.history[BLOCK..].copy_from_slice(block);
        s.time.copy_from_slice(&self.history);
        self.head = (self.head + n - 1) % n;
        let _ = p
            .fwd
            .process_with_scratch(&mut s.time, &mut self.fdl[self.head], &mut s.fwd);

        s.acc.fill(Complex::default());
        for (k, h) in self.kernel.parts.iter().enumerate() {
            let x = &self.fdl[(self.head + k) % n];
            for ((a, x), h) in s.acc.iter_mut().zip(x).zip(h) {
                *a += x * h;
            }
        }
        // A real signal's spectrum: DC and Nyquist are real.
        s.acc[0].im = 0.0;
        s.acc[BINS - 1].im = 0.0;
        let _ = p.inv.process_with_scratch(&mut s.acc, &mut s.out, &mut s.inv);
        block.copy_from_slice(&s.out[BLOCK..]);
    }

    pub fn reset(&mut self) {
        for f in &mut self.fdl {
            f.fill(Complex::default());
        }
        self.history.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain time-domain convolution, the reference.
    fn direct(x: &[f32], h: &[f32]) -> Vec<f32> {
        (0..x.len())
            .map(|n| {
                h.iter()
                    .enumerate()
                    .filter(|(k, _)| *k <= n)
                    .map(|(k, hv)| hv * x[n - k])
                    .sum()
            })
            .collect()
    }

    fn noise(len: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn matches_direct_convolution_across_blocks() {
        // Longer than two partitions, not a multiple of the block.
        let h = noise(2 * BLOCK + 123, 1);
        let x = noise(5 * BLOCK, 2);
        let want = direct(&x, &h);
        let mut c = Convolver::new(Arc::new(Kernel::new(&h)));
        let mut s = Scratch::default();
        let mut got = x.clone();
        for block in got.chunks_exact_mut(BLOCK) {
            c.process(block, &mut s);
        }
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!((g - w).abs() < 1e-3, "sample {i}: {g} vs {w}");
        }
    }

    #[test]
    fn identity_kernel_has_no_latency() {
        let mut c = Convolver::new(Arc::new(Kernel::new(&[1.0])));
        let mut s = Scratch::default();
        let x = noise(BLOCK, 3);
        let mut y = x.clone();
        c.process(&mut y, &mut s);
        for (a, b) in x.iter().zip(&y) {
            assert!((a - b).abs() < 1e-5);
        }
        c.reset();
        let mut z = vec![0.0; BLOCK];
        c.process(&mut z, &mut s);
        assert!(z.iter().all(|v| v.abs() < 1e-6), "reset clears the history");
    }
}
