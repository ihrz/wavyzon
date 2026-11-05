//! Impulse responses for `Convolution:`: decoded once (WAV, FLAC or Ogg
//! Vorbis, through symphonia), resampled to 48 kHz when needed, and
//! transformed into [`Kernel`]s. Runs at load time only, never on the audio
//! path.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use wavyzon_audio::dsp::Resampler;

use crate::conv::Kernel;

/// Longest impulse response kept, in seconds at 48 kHz (CPU per frame grows
/// with the length).
pub const MAX_SECONDS: usize = 10;
const MAX_FRAMES: usize = MAX_SECONDS * 48_000;
/// Largest impulse response file read.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// File extensions treated as impulse responses.
pub const EXTENSIONS: [&str; 4] = ["wav", "flac", "ogg", "oga"];

/// A decoded impulse response: one kernel per channel (at most two; APO
/// assigns IR channels to the selected channels round-robin).
#[derive(Debug)]
pub struct Impulse {
    pub channels: Vec<Arc<Kernel>>,
    /// Sample rate of the file before resampling.
    pub source_rate: u32,
    /// Length at 48 kHz, in samples.
    pub frames: usize,
    /// The file had more channels than the two kept.
    pub dropped_channels: usize,
    /// The file was longer than [`MAX_SECONDS`] and was cut.
    pub truncated: bool,
}

/// Decode `path` into per-channel samples at its own rate.
fn decode(path: &Path) -> Result<(u32, Vec<Vec<f32>>, usize), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {} MiB", MAX_FILE_BYTES / 1024 / 1024));
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported audio file: {e}"))?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("unknown sample rate")?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("unsupported codec: {e}"))?;
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut total_channels = 0;
    // Enough source frames to fill MAX_FRAMES at 48 kHz, plus a margin.
    let limit = MAX_FRAMES * rate as usize / 48_000 + 1024;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("read error: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode error: {e}")),
        };
        let spec = *decoded.spec();
        let n = spec.channels.count();
        if channels.is_empty() {
            total_channels = n;
            channels = vec![Vec::new(); n.min(2)];
        }
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks_exact(n) {
            for (c, ch) in channels.iter_mut().enumerate() {
                ch.push(frame[c]);
            }
        }
        if channels.first().is_some_and(|c| c.len() >= limit) {
            break;
        }
    }
    if channels.first().is_none_or(|c| c.is_empty()) {
        return Err("no audio".into());
    }
    Ok((rate, channels, total_channels))
}

/// Resample one or two channels to 48 kHz with the node's sinc resampler
/// (it takes 16-bit input, so the signal is scaled to full range first).
fn to_48k(rate: u32, channels: &[Vec<f32>]) -> Vec<Vec<f32>> {
    if rate == 48_000 {
        return channels.to_vec();
    }
    let peak = channels
        .iter()
        .flatten()
        .fold(0.0f32, |m, v| m.max(v.abs()))
        .max(f32::MIN_POSITIVE);
    let scale = 32_767.0 / peak;
    let len = channels[0].len();
    let mut interleaved = Vec::with_capacity(len * 2 + 128);
    for i in 0..len {
        let l = channels[0][i];
        let r = channels.get(1).map_or(l, |c| c[i]);
        interleaved.push((l * scale).round() as i16);
        interleaved.push((r * scale).round() as i16);
    }
    // Flush the filter's tail.
    interleaved.extend(std::iter::repeat_n(0i16, 128));
    let mut rs = Resampler::new(rate, 1.0);
    rs.write(&interleaved);
    let out_len = (len as f64 * 48_000.0 / rate as f64).ceil() as usize;
    let mut out = vec![0.0f64; out_len * 2];
    let n = rs.read(&mut out);
    let mut res = vec![Vec::with_capacity(n); channels.len()];
    for frame in out[..n * 2].chunks_exact(2) {
        for (c, ch) in res.iter_mut().enumerate() {
            ch.push((frame[c] / scale as f64) as f32);
        }
    }
    res
}

/// Load an impulse response file.
pub fn load(path: &Path) -> Result<Impulse, String> {
    let (rate, channels, total) = decode(path)?;
    let mut channels = to_48k(rate, &channels);
    let truncated = channels[0].len() > MAX_FRAMES;
    for c in &mut channels {
        c.truncate(MAX_FRAMES);
    }
    Ok(Impulse {
        frames: channels[0].len(),
        channels: channels.iter().map(|c| Arc::new(Kernel::new(c))).collect(),
        source_rate: rate,
        dropped_channels: total.saturating_sub(2),
        truncated,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Write a 16-bit PCM WAV file.
    pub(crate) fn write_wav(path: &Path, rate: u32, channels: &[Vec<f32>]) {
        let n = channels[0].len();
        let ch = channels.len() as u16;
        let data_len = (n * channels.len() * 2) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * ch as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(ch * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..n {
            for c in channels {
                b.extend_from_slice(&((c[i] * 32_767.0).round() as i16).to_le_bytes());
            }
        }
        std::fs::write(path, b).unwrap();
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wavyzon-apo-ir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_a_stereo_wav_at_48k() {
        let d = temp_dir("stereo");
        let p = d.join("ir.wav");
        let mut l = vec![0.0; 4800];
        let mut r = vec![0.0; 4800];
        l[0] = 0.9;
        r[10] = 0.5;
        write_wav(&p, 48_000, &[l, r]);
        let ir = load(&p).unwrap();
        assert_eq!(ir.channels.len(), 2);
        assert_eq!(ir.frames, 4800);
        assert_eq!(ir.source_rate, 48_000);
        assert_eq!(ir.channels[0].partitions(), 5);
        assert!(!ir.truncated);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resamples_other_rates() {
        let d = temp_dir("rate");
        let p = d.join("ir.wav");
        write_wav(&p, 44_100, &[vec![0.1; 4410]]);
        let ir = load(&p).unwrap();
        assert_eq!(ir.channels.len(), 1);
        assert_eq!(ir.source_rate, 44_100);
        assert!((ir.frames as i64 - 4800).abs() < 64, "{}", ir.frames);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_non_audio() {
        let d = temp_dir("bad");
        let p = d.join("ir.wav");
        std::fs::write(&p, b"not audio at all").unwrap();
        assert!(load(&p).is_err());
        assert!(load(&d.join("missing.wav")).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
}
