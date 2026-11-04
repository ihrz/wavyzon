//! Frame-level gain and mixing helpers.

use crate::{CHANNELS, FRAME_SAMPLES_PER_CHANNEL, Frame, clip};

/// `out = a * a_gain + b * b_gain`, saturating.
pub fn mix(a: &Frame, a_gain: f64, b: &Frame, b_gain: f64, out: &mut Frame) {
    for ((o, &x), &y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *o = clip(x as f64 * a_gain + y as f64 * b_gain);
    }
}

/// Apply a gain that moves linearly from `from` to `to` across the frame
/// (per stereo sample; the last sample gets exactly `to`).
pub fn ramp(f: &mut Frame, from: f64, to: f64) {
    for i in 0..FRAME_SAMPLES_PER_CHANNEL {
        let g = from + (to - from) * (i + 1) as f64 / FRAME_SAMPLES_PER_CHANNEL as f64;
        for c in 0..CHANNELS {
            let k = i * CHANNELS + c;
            f[k] = clip(f[k] as f64 * g);
        }
    }
}

/// `out = src * gain`, saturating (a plain copy at gain `1`).
pub fn scale(src: &Frame, gain: f64, out: &mut Frame) {
    if gain == 1.0 {
        *out = *src;
        return;
    }
    for (o, &s) in out.iter_mut().zip(src.iter()) {
        *o = clip(s as f64 * gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SILENCE;

    #[test]
    fn mix_clips() {
        let (mut a, mut b, mut out) = (SILENCE, SILENCE, SILENCE);
        a[0] = 30000;
        b[0] = 30000;
        a[1] = -30000;
        b[1] = -30000;
        mix(&a, 1.0, &b, 1.0, &mut out);
        assert_eq!((out[0], out[1]), (32767, -32768));
    }
}
