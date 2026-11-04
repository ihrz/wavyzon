use std::io::{self, Read};
use std::sync::Mutex;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use wavyzon_plugin::Format;

use super::CodecError;

/// Interleaved stereo `i16` PCM at the stream's native rate.
pub trait PcmStream: Send {
    /// Fill `out` (an even length) with interleaved stereo samples. Returns the
    /// number written; `Ok(0)` means end of stream.
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize>;
    /// Native sample rate (resample to 48 kHz downstream).
    fn sample_rate(&self) -> u32;
}

/// Makes any `Read + Send` usable where Symphonia wants `Sync`.
struct Locked<R>(Mutex<R>);

impl<R: Read> Read for Locked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.get_mut().map_err(|_| io::Error::other("poisoned"))?.read(buf)
    }
}

/// Open a decoder for `format` over a blocking reader.
///
/// This probes the container, so it reads from `reader`: call it off the
/// async runtime (for example in `spawn_blocking`).
pub fn decode(format: &Format, reader: Box<dyn Read + Send>) -> Result<Box<dyn PcmStream>, CodecError> {
    match format {
        Format::Pcm48 => Ok(Box::new(Pcm48 {
            r: io::BufReader::with_capacity(64 << 10, reader),
            raw: Vec::new(),
        })),
        Format::Encoded { hint } => Ok(Box::new(Probed::open(hint.as_deref(), reader)?)),
    }
}

struct Pcm48 {
    r: io::BufReader<Box<dyn Read + Send>>,
    raw: Vec<u8>,
}

impl PcmStream for Pcm48 {
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
        let need = out.len() * 2;
        self.raw.resize(need, 0);
        let mut filled = 0;
        while filled < need {
            match self.r.read(&mut self.raw[filled..need]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let count = filled / 2;
        for (i, s) in out.iter_mut().take(count).enumerate() {
            *s = i16::from_le_bytes([self.raw[2 * i], self.raw[2 * i + 1]]);
        }
        Ok(count & !1)
    }

    fn sample_rate(&self) -> u32 {
        48_000
    }
}

struct Probed {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    rate: u32,
    pending: Vec<i16>,
    offset: usize,
    done: bool,
}

// Symphonia's readers and decoders are Send; the trait objects just don't say so.
unsafe impl Send for Probed {}

impl Probed {
    fn open(hint: Option<&str>, reader: Box<dyn Read + Send>) -> Result<Self, CodecError> {
        let src = ReadOnlySource::new(Locked(Mutex::new(reader)));
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut h = Hint::new();
        if let Some(hint) = hint {
            if hint.contains('/') {
                h.mime_type(hint);
            } else {
                h.with_extension(hint.trim_start_matches('.'));
            }
        }
        let probed = symphonia::default::get_probe()
            .format(
                &h,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| CodecError::Unsupported(e.to_string()))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| CodecError::Unsupported("no audio track".into()))?;
        let rate = track.codec_params.sample_rate.unwrap_or(48_000);
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| CodecError::Unsupported(e.to_string()))?;
        Ok(Self {
            format,
            decoder,
            track_id,
            rate,
            pending: Vec::new(),
            offset: 0,
            done: false,
        })
    }

    /// Decode the next packet into `pending`. Returns false at end of stream.
    fn refill(&mut self) -> io::Result<bool> {
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::IoError(e)) => return Err(e),
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(io::Error::other(e.to_string())),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymError::DecodeError(_)) => {
                    bad += 1;
                    if bad > 64 {
                        return Err(io::Error::other("too many corrupt packets"));
                    }
                    continue;
                }
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(e) => return Err(io::Error::other(e.to_string())),
            };
            let spec = *decoded.spec();
            if spec.rate > 0 {
                self.rate = spec.rate;
            }
            let channels = spec.channels.count().max(1);
            let mut buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
            buf.copy_interleaved_ref(decoded);
            let s = buf.samples();
            self.pending.clear();
            self.offset = 0;
            match channels {
                1 => {
                    for v in s {
                        self.pending.push(*v);
                        self.pending.push(*v);
                    }
                }
                2 => self.pending.extend_from_slice(s),
                n => {
                    for chunk in s.chunks_exact(n) {
                        self.pending.push(chunk[0]);
                        self.pending.push(chunk[1]);
                    }
                }
            }
            if !self.pending.is_empty() {
                return Ok(true);
            }
        }
    }
}

impl PcmStream for Probed {
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
        let mut written = 0;
        while written < out.len() {
            if self.offset >= self.pending.len() && (self.done || !self.refill()?) {
                self.done = true;
                break;
            }
            let n = (self.pending.len() - self.offset).min(out.len() - written);
            out[written..written + n].copy_from_slice(&self.pending[self.offset..self.offset + n]);
            self.offset += n;
            written += n;
        }
        Ok(written)
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        v.extend_from_slice(&(channels * 2).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    #[test]
    fn pcm48_reads_little_endian() {
        let raw: Vec<u8> = [1i16, -2, 3, -4].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut d = decode(&Format::Pcm48, Box::new(io::Cursor::new(raw))).unwrap();
        let mut out = [0i16; 8];
        assert_eq!(d.read(&mut out).unwrap(), 4);
        assert_eq!(&out[..4], &[1, -2, 3, -4]);
        assert_eq!(d.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn wav_mono_is_upmixed() {
        let samples: Vec<i16> = (0..4410).map(|i| (i % 100) as i16).collect();
        let mut d = decode(
            &Format::Encoded {
                hint: Some("wav".into()),
            },
            Box::new(io::Cursor::new(wav(44_100, 1, &samples))),
        )
        .unwrap();
        assert_eq!(d.sample_rate(), 44_100);
        let mut out = vec![0i16; 20_000];
        let mut total = 0;
        loop {
            let n = d.read(&mut out[total..]).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, samples.len() * 2);
        assert_eq!(out[2], 1);
        assert_eq!(out[3], 1);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(
            decode(
                &Format::Encoded { hint: None },
                Box::new(io::Cursor::new(vec![7u8; 512]))
            )
            .is_err()
        );
    }
}
