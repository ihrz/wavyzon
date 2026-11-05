//! Filter design: biquad coefficients for APO's filter types (RBJ "Audio EQ
//! Cookbook" formulas) and the minimum-phase FIR of a `GraphicEQ:` curve.

use std::f64::consts::{LN_2, PI};

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use crate::parse::{BiquadSpec, Kind};

pub const SAMPLE_RATE: f64 = 48_000.0;
/// Q used when a filter that accepts one doesn't give it.
pub const DEFAULT_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// Q of a notch without Q or BW: a narrow notch, as REW and DCX-style
/// notches are (the APO reference doesn't state its default).
pub const DEFAULT_NOTCH_Q: f64 = 30.0;

/// Normalised biquad coefficients (`a0 = 1`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coefs {
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Magnitude response in dB at `hz` (for tests and summaries).
    pub fn response_db(&self, hz: f64) -> f64 {
        let w = 2.0 * PI * hz / SAMPLE_RATE;
        let z1 = Complex::from_polar(1.0, -w);
        let z2 = z1 * z1;
        let num = Complex::new(self.b0, 0.0) + z1 * self.b1 + z2 * self.b2;
        let den = Complex::new(1.0, 0.0) + z1 * self.a1 + z2 * self.a2;
        20.0 * (num.norm() / den.norm()).log10()
    }
}

/// Bandwidth in octaves → Q (RBJ's relation for the digital filter).
fn q_from_bw(bw: f64, w0: f64) -> f64 {
    1.0 / (2.0 * (LN_2 / 2.0 * bw * w0 / w0.sin()).sinh())
}

/// Design one `Filter:` line. `fc` is kept below Nyquist.
pub fn biquad(spec: &BiquadSpec) -> Coefs {
    let fc = spec.fc.clamp(1.0, SAMPLE_RATE / 2.0 - 1.0);
    let gain = spec.gain_db;
    // 12 dB corner shelves: move the RBJ midpoint so the poles (the corner)
    // land on fc (RBJ low shelf poles sit at w0 / sqrt(A)).
    let a_amp = 10f64.powf(gain / 40.0);
    let f0 = match spec.kind {
        Kind::LowShelf12 => fc * a_amp.sqrt(),
        Kind::HighShelf12 => fc / a_amp.sqrt(),
        _ => fc,
    }
    .clamp(1.0, SAMPLE_RATE / 2.0 - 1.0);
    let w0 = 2.0 * PI * f0 / SAMPLE_RATE;
    let (sin, cos) = w0.sin_cos();
    let q = match (spec.q, spec.bw_oct) {
        (Some(q), _) => q,
        (None, Some(bw)) => q_from_bw(bw, w0),
        (None, None) if spec.kind == Kind::Notch => DEFAULT_NOTCH_Q,
        (None, None) => DEFAULT_Q,
    };
    let alpha = sin / (2.0 * q);
    match spec.kind {
        Kind::Peaking => Coefs::new(
            1.0 + alpha * a_amp,
            -2.0 * cos,
            1.0 - alpha * a_amp,
            1.0 + alpha / a_amp,
            -2.0 * cos,
            1.0 - alpha / a_amp,
        ),
        Kind::LowPass => Coefs::new(
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        Kind::HighPass => Coefs::new(
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        Kind::BandPass => Coefs::new(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        Kind::Notch => Coefs::new(1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        Kind::AllPass => Coefs::new(
            1.0 - alpha,
            -2.0 * cos,
            1.0 + alpha,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        Kind::LowShelf | Kind::HighShelf | Kind::LowShelf12 | Kind::HighShelf12 => {
            let alpha = match (spec.slope_db, spec.q, spec.bw_oct) {
                // Shelf slope S: 1 = the steepest monotonic slope, 12 dB/oct.
                (Some(db), None, None) => {
                    let s = db / 12.0;
                    sin / 2.0 * ((a_amp + 1.0 / a_amp) * (1.0 / s - 1.0) + 2.0).max(0.0).sqrt()
                }
                _ => alpha,
            };
            let a = a_amp;
            let sq = 2.0 * a.sqrt() * alpha;
            if matches!(spec.kind, Kind::LowShelf | Kind::LowShelf12) {
                Coefs::new(
                    a * ((a + 1.0) - (a - 1.0) * cos + sq),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - sq),
                    (a + 1.0) + (a - 1.0) * cos + sq,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - sq,
                )
            } else {
                Coefs::new(
                    a * ((a + 1.0) + (a - 1.0) * cos + sq),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - sq),
                    (a + 1.0) - (a - 1.0) * cos + sq,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - sq,
                )
            }
        }
        // First-order shelves, bilinear transform of (s + G·wc)/(s + wc)
        // (low) and (G·s + wc)/(s + wc) (high), wc pre-warped to fc.
        Kind::LowShelf6 | Kind::HighShelf6 => {
            let g = 10f64.powf(gain / 20.0);
            let k = (PI * fc / SAMPLE_RATE).tan();
            if spec.kind == Kind::LowShelf6 {
                Coefs::new(1.0 + g * k, g * k - 1.0, 0.0, 1.0 + k, k - 1.0, 0.0)
            } else {
                Coefs::new(g + k, k - g, 0.0, 1.0 + k, k - 1.0, 0.0)
            }
        }
    }
}

/// FFT size of the GraphicEQ design (2.9 Hz bins at 48 kHz).
const GEQ_FFT: usize = 16_384;
/// Taps kept from the minimum-phase response.
pub const GEQ_TAPS: usize = 8_192;

/// The gain of a GraphicEQ curve at `hz`: linear in log-frequency between
/// points, flat (the end point's gain) outside them, as APO describes it.
pub fn graphic_eq_gain(points: &[(f64, f64)], hz: f64) -> f64 {
    let (first, last) = (points[0], points[points.len() - 1]);
    if hz <= first.0 {
        return first.1;
    }
    if hz >= last.0 {
        return last.1;
    }
    let i = points.partition_point(|p| p.0 <= hz);
    let (lo, hi) = (points[i - 1], points[i]);
    if hi.0 <= lo.0 {
        return hi.1;
    }
    let t = (hz.ln() - lo.0.ln()) / (hi.0.ln() - lo.0.ln());
    lo.1 + (hi.1 - lo.1) * t
}

/// A minimum-phase FIR following a GraphicEQ curve (homomorphic method:
/// fold the real cepstrum of the log magnitude). Minimum phase adds no
/// latency, so the wire clock stays exact, and behaves like APO's IIR-style
/// EQs. Returns `None` for a flat curve.
pub fn graphic_eq_taps(points: &[(f64, f64)]) -> Option<Vec<f32>> {
    if points.iter().all(|p| p.1 == 0.0) {
        return None;
    }
    let n = GEQ_FFT;
    let bins = n / 2 + 1;
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);

    // ln |H| on the bins (the curve is in dB).
    let mut spec: Vec<Complex<f64>> = (0..bins)
        .map(|k| {
            let hz = (k as f64 * SAMPLE_RATE / n as f64).max(1.0);
            Complex::new(graphic_eq_gain(points, hz) * std::f64::consts::LN_10 / 20.0, 0.0)
        })
        .collect();
    let mut cep = inv.make_output_vec();
    inv.process(&mut spec, &mut cep).ok()?;
    for v in &mut cep {
        *v /= n as f64;
    }
    // Fold: keep c[0] and c[n/2], double the causal part, drop the rest.
    for (i, v) in cep.iter_mut().enumerate() {
        if i == 0 || i == n / 2 {
            continue;
        }
        *v = if i < n / 2 { *v * 2.0 } else { 0.0 };
    }
    let mut mp = fwd.make_output_vec();
    fwd.process(&mut cep, &mut mp).ok()?;
    for v in &mut mp {
        *v = v.exp();
    }
    mp[0].im = 0.0;
    mp[bins - 1].im = 0.0;
    let mut h = inv.make_output_vec();
    inv.process(&mut mp, &mut h).ok()?;
    // Keep the head; fade the last eighth out so truncation doesn't ring.
    let fade = GEQ_TAPS / 8;
    Some(
        (0..GEQ_TAPS)
            .map(|i| {
                let mut v = h[i] / n as f64;
                if i >= GEQ_TAPS - fade {
                    let t = (i - (GEQ_TAPS - fade)) as f64 / fade as f64;
                    v *= 0.5 * (1.0 + (PI * t).cos());
                }
                v as f32
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: Kind, fc: f64, gain_db: f64, q: Option<f64>) -> BiquadSpec {
        BiquadSpec {
            kind,
            fc,
            gain_db,
            q,
            bw_oct: None,
            slope_db: None,
        }
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn peaking_hits_its_gain_at_fc() {
        let c = biquad(&spec(Kind::Peaking, 1000.0, 6.0, Some(1.0)));
        assert!(close(c.response_db(1000.0), 6.0, 1e-6));
        assert!(close(c.response_db(20.0), 0.0, 0.05));
        assert!(close(c.response_db(20_000.0), 0.0, 0.1));
        let cut = biquad(&spec(Kind::Peaking, 100.0, -10.0, Some(2.5)));
        assert!(close(cut.response_db(100.0), -10.0, 1e-6));
    }

    #[test]
    fn passes_and_notch() {
        let lp = biquad(&spec(Kind::LowPass, 1000.0, 0.0, None));
        assert!(close(lp.response_db(1000.0), -3.01, 0.01));
        assert!(close(lp.response_db(20.0), 0.0, 0.01));
        assert!(lp.response_db(10_000.0) < -35.0);
        let hp = biquad(&spec(Kind::HighPass, 100.0, 0.0, None));
        assert!(close(hp.response_db(100.0), -3.01, 0.01));
        assert!(hp.response_db(20.0) < -25.0);
        let bp = biquad(&spec(Kind::BandPass, 1000.0, 0.0, Some(1.0)));
        assert!(close(bp.response_db(1000.0), 0.0, 1e-6));
        let no = biquad(&spec(Kind::Notch, 800.0, 0.0, None));
        assert!(no.response_db(800.0) < -60.0);
        assert!(close(no.response_db(700.0), 0.0, 0.5), "default notch is narrow");
        let ap = biquad(&spec(Kind::AllPass, 900.0, 0.0, Some(0.707)));
        for hz in [50.0, 900.0, 5000.0] {
            assert!(close(ap.response_db(hz), 0.0, 1e-6));
        }
    }

    #[test]
    fn shelves_reach_their_plateau() {
        for kind in [Kind::LowShelf, Kind::LowShelf6, Kind::LowShelf12] {
            let c = biquad(&spec(kind, 300.0, 6.0, None));
            assert!(close(c.response_db(10.0), 6.0, 0.2), "{kind:?} {}", c.response_db(10.0));
            assert!(close(c.response_db(15_000.0), 0.0, 0.2), "{kind:?}");
        }
        for kind in [Kind::HighShelf, Kind::HighShelf6, Kind::HighShelf12] {
            let c = biquad(&spec(kind, 1000.0, -6.0, None));
            assert!(close(c.response_db(10.0), 0.0, 0.2), "{kind:?}");
            assert!(
                close(c.response_db(23_000.0), -6.0, 0.5),
                "{kind:?} {}",
                c.response_db(23_000.0)
            );
        }
        // RBJ shelf: half the gain (in dB) at the midpoint.
        let mid = biquad(&spec(Kind::LowShelf, 300.0, 6.0, None));
        assert!(close(mid.response_db(300.0), 3.0, 0.01));
        // Corner shelves: 3 dB short of the plateau at the corner.
        let corner = biquad(&spec(Kind::LowShelf12, 300.0, 12.0, None));
        assert!(
            close(corner.response_db(300.0), 9.0, 0.5),
            "{}",
            corner.response_db(300.0)
        );
        let slope = biquad(&BiquadSpec {
            slope_db: Some(12.0),
            ..spec(Kind::LowShelf, 300.0, 6.0, None)
        });
        assert!(close(slope.response_db(300.0), 3.0, 0.01));
    }

    #[test]
    fn bandwidth_converts_to_q() {
        let by_bw = biquad(&BiquadSpec {
            bw_oct: Some(1.0),
            ..spec(Kind::Peaking, 1000.0, 6.0, None)
        });
        let by_q = biquad(&spec(Kind::Peaking, 1000.0, 6.0, Some(std::f64::consts::SQRT_2)));
        assert!(close(by_bw.response_db(1500.0), by_q.response_db(1500.0), 0.05));
    }

    #[test]
    fn graphic_eq_curve_interpolates_in_log_frequency() {
        let pts = [(100.0, 0.0), (1000.0, 6.0)];
        assert_eq!(graphic_eq_gain(&pts, 50.0), 0.0);
        assert_eq!(graphic_eq_gain(&pts, 5000.0), 6.0);
        assert!(close(graphic_eq_gain(&pts, 316.227_766), 3.0, 1e-6));
        assert!(graphic_eq_taps(&[(100.0, 0.0), (200.0, 0.0)]).is_none());
    }

    #[test]
    fn graphic_eq_fir_follows_the_curve() {
        let pts = [(100.0, -6.0), (1000.0, -6.0), (2000.0, 6.0), (8000.0, 6.0)];
        let h = graphic_eq_taps(&pts).unwrap();
        assert_eq!(h.len(), GEQ_TAPS);
        // Measure the FIR's response with a long zero-padded FFT.
        let n = 65_536;
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let mut x = fft.make_input_vec();
        for (i, v) in h.iter().enumerate() {
            x[i] = *v as f64;
        }
        let mut y = fft.make_output_vec();
        fft.process(&mut x, &mut y).unwrap();
        let at = |hz: f64| 20.0 * y[(hz * n as f64 / SAMPLE_RATE).round() as usize].norm().log10();
        for (hz, want) in [
            (300.0, -6.0),
            (1414.0, 0.0),
            (4000.0, 6.0),
            (15_000.0, 6.0),
            (50.0, -6.0),
        ] {
            assert!(close(at(hz), want, 0.3), "{hz} Hz: {} dB, want {want}", at(hz));
        }
        // Minimum phase: the energy is at the start, not centred.
        let energy: f64 = h.iter().map(|v| (*v as f64).powi(2)).sum();
        let head: f64 = h[..512].iter().map(|v| (*v as f64).powi(2)).sum();
        assert!(head / energy > 0.95);
    }
}
