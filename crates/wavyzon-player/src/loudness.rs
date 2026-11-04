//! Loudness normalisation from offline measurements.

use crate::traits::Loudness;

const NORM_MAX_CUT_DB: f64 = -12.0;
const NORM_MAX_BOOST_DB: f64 = 6.0;
/// Highest peak a boost may produce (-1 dBFS).
pub(crate) const NORM_PEAK_CEIL: f64 = 0.891;
const NORM_FLOOR_LUFS: f64 = -70.0;

/// Linear gain that brings `l` to `target_lufs`: cuts at most 12 dB, boosts
/// at most 6 dB, never pushes the peak past -1 dBFS, and leaves silent or
/// unmeasurable tracks alone.
pub fn normalization_gain(l: Loudness, target_lufs: f64) -> f64 {
    if !l.integrated_lufs.is_finite() || l.integrated_lufs < NORM_FLOOR_LUFS {
        return 1.0;
    }
    let db = (target_lufs - l.integrated_lufs).clamp(NORM_MAX_CUT_DB, NORM_MAX_BOOST_DB);
    let mut gain = 10f64.powf(db / 20.0);
    if l.peak_amplitude > 0.0 {
        gain = gain.min(NORM_PEAK_CEIL / l.peak_amplitude);
    }
    gain
}
