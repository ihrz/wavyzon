//! Range-based seeking into Deezer media, port of Cadence's
//! `internal/source/deezer/seek.go`.
//!
//! Deezer serves constant-bitrate MP3, so a `start_ms` maps to a byte offset
//! with simple arithmetic. The offset is snapped to a stripe-chunk boundary
//! (only whole 2048-byte chunks can be decrypted), a small window around it is
//! fetched with a `Range` request, decrypted from that chunk index, and the
//! first real MPEG frame after the estimate becomes the stream start.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use wavyzon_plugin::{Failure, Format, Stream, Track};

use super::stripe::{STRIPE_CHUNK, StripeDecryptor};
use super::{Deezer, LANG, NAME, session::MediaLink};
use crate::cache::Lru;
use crate::httpx::Req;
use crate::metrics::inc;
use crate::util::Body;

/// Samples per second of a [`Format::Pcm48`] stream (the unit of
/// [`Stream::start_sample`]).
const SAMPLE_RATE_48K: i64 = 48_000;

/// Seeks closer than this stream from the start instead (Cadence's
/// `seekMinMs`).
pub const SEEK_MIN_MS: i64 = 10_000;
/// Frames decoded before the target to warm the decoder (Cadence's
/// `seekMarginFrames`).
const SEEK_MARGIN_FRAMES: i64 = 4;
/// Bytes of layout probe (Cadence's `layoutProbeBytes`).
const LAYOUT_PROBE_BYTES: usize = 8 * STRIPE_CHUNK;
/// How far the frame sync may be from the estimate (Cadence's `syncSearch`).
const SYNC_SEARCH: i64 = 32;
/// Extra bytes read around the estimate to find a frame (Cadence's
/// `seekPrefixBytes`).
const SEEK_PREFIX_BYTES: i64 = 4096;
/// Cached layouts (Cadence's `layoutEntries`). A layout never changes for a
/// given track and format, so entries do not expire.
const LAYOUT_ENTRIES: usize = 50_000;
const LAYOUT_TTL: Duration = Duration::from_secs(365 * 24 * 3600);
/// Samples per MPEG-1 Layer III frame (Cadence's `mpeg1Layer3SPF`).
const MPEG1_LAYER3_SPF: i64 = 1152;

const BITRATES_V1_L3: [i64; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
const RATES_V1: [i64; 4] = [44100, 48000, 32000, 0];

const ERR_NOT_CBR: &str = "stream is not constant-bitrate mpeg-1 layer iii";
const ERR_NO_FRAME_SYNC: &str = "no frame header at the computed offset";

#[derive(Debug, Clone, Copy, PartialEq)]
struct FrameHeader {
    bitrate: i64,
    rate: i64,
    size: i64,
    mono: bool,
    crc: bool,
}

/// The CBR MP3 layout of one stream (Cadence's `mp3Layout`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mp3Layout {
    data_start: i64,
    bitrate: i64,
    rate: i64,
    frame_bytes: f64,
}

impl Mp3Layout {
    /// Byte offset of `frame` (Cadence's `offset`).
    fn offset(&self, frame: i64) -> i64 {
        self.data_start + (frame as f64 * self.frame_bytes).floor() as i64
    }

    /// Position of the first frame at (or within [`SYNC_SEARCH`] of) `at`
    /// (Cadence's `findFrame`).
    fn find_frame(&self, buf: &[u8], at: i64) -> Option<i64> {
        for d in 0..=SYNC_SEARCH {
            for pos in [at - d, at + d] {
                if pos < 0 || pos + 4 > buf.len() as i64 {
                    continue;
                }
                let Some(h) = parse_frame_header(&buf[pos as usize..]) else {
                    continue;
                };
                if h.bitrate != self.bitrate || h.rate != self.rate {
                    continue;
                }
                let next_at = (pos + h.size) as usize;
                if let Some(next) = buf.get(next_at..).and_then(parse_frame_header) {
                    if next.bitrate == self.bitrate && next.rate == self.rate {
                        return Some(pos);
                    }
                }
            }
        }
        None
    }
}

/// Parse one MPEG-1 Layer III frame header (Cadence's `parseFrameHeader`).
fn parse_frame_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xFE != 0xFA {
        return None;
    }
    let bitrate = *BITRATES_V1_L3.get((b[2] >> 4) as usize)?;
    let rate = *RATES_V1.get(((b[2] >> 2) & 3) as usize)?;
    if bitrate == 0 || rate == 0 {
        return None;
    }
    let pad = i64::from((b[2] >> 1) & 1);
    Some(FrameHeader {
        bitrate,
        rate,
        size: 144 * bitrate * 1000 / rate + pad,
        mono: b[3] >> 6 == 3,
        crc: b[1] & 1 == 0,
    })
}

/// Read the layout out of the first frames of a decrypted probe (Cadence's
/// `parseLayout`). VBR (Xing) streams are rejected.
fn parse_layout(p: &[u8]) -> Result<Mp3Layout, String> {
    let mut start = 0usize;
    if p.len() >= 10 && &p[..3] == b"ID3" {
        start = 10
            + (((p[6] & 0x7F) as usize) << 21
                | ((p[7] & 0x7F) as usize) << 14
                | ((p[8] & 0x7F) as usize) << 7
                | (p[9] & 0x7F) as usize);
        if p[5] & 0x10 != 0 {
            start += 10;
        }
    }
    if start + 40 > p.len() {
        return Err(format!("{ERR_NOT_CBR}: header beyond the probed {} bytes", p.len()));
    }
    let h = parse_frame_header(&p[start..]).ok_or_else(|| ERR_NOT_CBR.to_string())?;
    let mut side = 32;
    if h.mono {
        side = 17;
    }
    if h.crc {
        side += 2;
    }
    if p.get(start + 4 + side..start + 8 + side) == Some(b"Xing") {
        return Err(format!("{ERR_NOT_CBR}: xing vbr header"));
    }
    let next = p
        .get((start + h.size as usize).min(p.len())..)
        .and_then(parse_frame_header);
    match next {
        Some(n) if n.bitrate == h.bitrate && n.rate == h.rate => {}
        _ => return Err(format!("{ERR_NOT_CBR}: second frame does not match the first")),
    }
    Ok(Mp3Layout {
        data_start: start as i64,
        bitrate: h.bitrate,
        rate: h.rate,
        frame_bytes: 144.0 * h.bitrate as f64 * 1000.0 / h.rate as f64,
    })
}

/// The total size out of a `Content-Range: bytes a-b/total` header.
fn range_total(header: Option<&str>) -> i64 {
    let Some((_, total)) = header.and_then(|h| h.split_once('/')) else {
        return 0;
    };
    total.trim().parse().unwrap_or(0).max(0)
}

/// Read up to `buf.len()` bytes, returning how many arrived (a short read at
/// EOF is fine, like Go's `io.ErrUnexpectedEOF`).
async fn read_full<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]).await {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl Deezer {
    pub(crate) fn new_layout_cache() -> Lru<String, Mp3Layout> {
        Lru::new(LAYOUT_ENTRIES, LAYOUT_TTL)
    }

    /// `Range: bytes=<offset>-` on the CDN, tolerating a CDN that ignores
    /// ranges by skipping the bytes in between (Cadence's `rangeFrom`).
    pub(crate) async fn range_from(&self, url: &str, offset: i64) -> Result<(Body, i64), Failure> {
        let req = Req::get(url)
            .map_err(|e| e.to_failure(NAME))?
            .header("Range", &format!("bytes={offset}-"))
            .header(LANG.0, LANG.1);
        let res = self.http.stream(&req).await.map_err(|e| e.to_failure(NAME))?;
        let status = res.status().as_u16();
        let total = res.content_length().unwrap_or(0) as i64;
        if status == 206 {
            let total = range_total(
                res.headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok()),
            );
            return Ok((crate::util::response_body(res), total));
        }
        inc(&self.metrics.cdn_range_ignored);
        tracing::warn!(
            offset,
            status,
            "cdn ignored a range request, re-downloading the skipped bytes"
        );
        let mut skipper = crate::util::response_body(res).take(offset as u64);
        let skipped = tokio::io::copy(&mut skipper, &mut tokio::io::sink())
            .await
            .map_err(|e| Failure::common(NAME, "skip cached head", Some(e.to_string())))?;
        if skipped != offset as u64 {
            return Err(Failure::common(
                NAME,
                "stream ended while skipping to the seek offset",
                None,
            ));
        }
        Ok((skipper.into_inner(), total))
    }

    /// Decrypt `LAYOUT_PROBE_BYTES` from the start and parse the layout
    /// (Cadence's `probeLayout`).
    async fn probe_layout(
        &self,
        track_id: &str,
        link: &MediaLink,
        dec: &StripeDecryptor,
    ) -> Result<Mp3Layout, Failure> {
        let key = format!("{track_id}-{}", link.format.to_ascii_lowercase());
        if let Some(l) = self.layouts.get(&key) {
            return Ok(l);
        }
        let (body, _) = self.range_from(&link.url, 0).await?;
        let capped: Body = Box::new(body.take(LAYOUT_PROBE_BYTES as u64));
        let mut plain = dec.decrypt_from(track_id, capped, 0);
        let mut probe = vec![0u8; LAYOUT_PROBE_BYTES];
        let n = read_full(&mut plain, &mut probe)
            .await
            .map_err(|e| Failure::common(NAME, "layout probe read failed", Some(e.to_string())))?;
        let l = parse_layout(&probe[..n]).map_err(|m| Failure::common(NAME, m, None))?;
        self.layouts.set(key, l);
        Ok(l)
    }

    /// Open the stream at `start_ms` with a range request (Cadence's
    /// `seekOpen`). The caller falls back to a full stream on error.
    pub(crate) async fn seek_open(
        &self,
        track: &Track,
        start_ms: i64,
        dec: &StripeDecryptor,
    ) -> Result<Stream, Failure> {
        let link = self.media_url(&track.identifier).await?;
        let layout = self.probe_layout(&track.identifier, &link, dec).await?;
        let target = start_ms * layout.rate / 1000 / MPEG1_LAYER3_SPF;
        let first = target - SEEK_MARGIN_FRAMES;
        if first < 1 {
            return Err(Failure::common(NAME, "target is inside the first frames", None));
        }
        let est = layout.offset(first);
        let chunk_start = (est - SYNC_SEARCH).max(0) / STRIPE_CHUNK as i64 * STRIPE_CHUNK as i64;
        let (body, _) = self.range_from(&link.url, chunk_start).await?;
        let mut plain = dec.decrypt_from(&track.identifier, body, (chunk_start / STRIPE_CHUNK as i64) as usize);
        let mut prefix = vec![0u8; (est - chunk_start + SEEK_PREFIX_BYTES) as usize];
        let n = read_full(&mut plain, &mut prefix)
            .await
            .map_err(|e| Failure::common(NAME, "seek read failed", Some(e.to_string())))?;
        let pos = layout
            .find_frame(&prefix[..n], est - chunk_start)
            .ok_or_else(|| Failure::common(NAME, ERR_NO_FRAME_SYNC, None))?;
        let head: Body = Box::new(std::io::Cursor::new(prefix[pos as usize..n].to_vec()).chain(plain));
        inc(&self.metrics.seek.range);
        Ok(Stream {
            body: head,
            format: Format::mp3(),
            // The player skips back to `start_ms` from the warm-up frames.
            start_sample: first * MPEG1_LAYER3_SPF * SAMPLE_RATE_48K / layout.rate,
            live: false,
        })
    }
}

/// Build the "stream unavailable" failure the full open returns when
/// decryption is off (Cadence's `ErrNoDecryptor`).
pub(crate) fn no_decryptor() -> Failure {
    Failure::fault(
        NAME,
        "stream unavailable",
        Some(super::session::NO_DECRYPTOR.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deezer::tests_support::synth_mp3;

    #[test]
    fn layout_parses_cbr_and_rejects_vbr() {
        let l = parse_layout(&synth_mp3(10, false)).unwrap();
        assert_eq!(l.data_start, 110);
        assert_eq!(l.bitrate, 128);
        assert_eq!(l.rate, 44100);
        assert!(parse_layout(&synth_mp3(10, true)).is_err(), "vbr file accepted");
    }

    #[test]
    fn frame_header_needs_an_mpeg1_layer3_sync() {
        assert!(parse_frame_header(&[0xFF, 0xFB, 0x90, 0x00]).is_some());
        assert!(parse_frame_header(&[0xFF, 0xFA, 0x90, 0x00]).is_some(), "crc variant");
        assert!(
            parse_frame_header(&[0xFF, 0xF3, 0x90, 0x00]).is_none(),
            "reserved layer rejected"
        );
        assert!(
            parse_frame_header(&[0xFF, 0xFB, 0x00, 0x00]).is_none(),
            "free bitrate rejected"
        );
    }

    #[test]
    fn offsets_and_frames_are_linear() {
        let l = parse_layout(&synth_mp3(50, false)).unwrap();
        assert_eq!(l.offset(0), 110);
        assert_eq!(l.offset(10) - l.offset(9), l.frame_bytes.floor() as i64 + 1);
    }

    #[test]
    fn find_frame_recovers_a_displaced_estimate() {
        let l = parse_layout(&synth_mp3(20, false)).unwrap();
        let data = synth_mp3(20, false);
        let at = l.offset(5) - l.data_start;
        for shift in [-8i64, 0, 8] {
            let pos = l.find_frame(&data[l.data_start as usize..], at + shift).unwrap();
            assert_eq!(pos, at, "shift {shift}");
        }
        assert!(l.find_frame(&data[l.data_start as usize..], at + 512).is_none());
    }

    #[test]
    fn range_total_reads_the_content_range() {
        assert_eq!(range_total(Some("bytes 0-99/12345")), 12345);
        assert_eq!(range_total(Some("bytes 0-99/*")), 0);
        assert_eq!(range_total(None), 0);
    }

    #[tokio::test]
    async fn read_full_tolerates_a_short_eof() {
        let mut body: Body = Box::new(std::io::Cursor::new(vec![1u8, 2, 3]));
        let mut buf = [0u8; 8];
        assert_eq!(read_full(&mut body, &mut buf).await.unwrap(), 3);
    }
}
