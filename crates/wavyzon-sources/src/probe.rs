//! Cheap format sniffing shared by the HTTP and local sources: a decoder hint
//! from a MIME type or file extension, and a duration estimate from the first
//! bytes of MP3 (CBR estimate from the first frame) and WAV (exact, from the
//! header) files.

/// Bytes read to sniff a file.
pub const SNIFF_BYTES: usize = 16 * 1024;

/// File extensions treated as audio by the local source and as a fallback
/// hint by the HTTP source.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "mp4", "aac", "flac", "ogg", "oga", "opus", "wav", "webm", "mka",
];

/// A decoder hint (file extension) for a MIME type, ignoring parameters.
pub fn hint_for_mime(content_type: &str) -> Option<&'static str> {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => "mp3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "aac",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "video/mp4" => "m4a",
        "audio/ogg" | "application/ogg" | "audio/vorbis" | "audio/opus" | "audio/x-opus" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => "wav",
        "audio/webm" | "video/webm" => "webm",
        _ => return None,
    })
}

/// The lowercase extension of the last path segment, if it is an audio one.
pub fn hint_for_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    AUDIO_EXTENSIONS.iter().copied().find(|e| *e == ext).map(|e| match e {
        "oga" | "opus" => "ogg",
        "mp4" => "m4a",
        "mka" => "webm",
        other => other,
    })
}

/// What a sniff learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sniffed {
    /// Estimated duration, 0 when unknown.
    pub duration_ms: i64,
    /// Offset of the first audio byte (after ID3v2 / WAV header).
    pub audio_start: u64,
    /// Whether starting at an arbitrary byte offset still decodes (MP3, ADTS).
    pub byte_seekable: bool,
}

/// Sniff `head` (the first bytes of a file of `total` bytes, if known).
pub fn sniff(head: &[u8], total: Option<u64>, hint: Option<&str>) -> Sniffed {
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return sniff_wav(head, total);
    }
    if head.starts_with(b"ID3") || hint == Some("mp3") || (hint.is_none() && frame_bitrate(head).is_some()) {
        return sniff_mp3(head, total);
    }
    Sniffed {
        byte_seekable: hint == Some("aac"),
        ..Default::default()
    }
}

fn id3_size(head: &[u8]) -> u64 {
    if head.len() < 10 || !head.starts_with(b"ID3") {
        return 0;
    }
    let s = &head[6..10];
    if s.iter().any(|b| b & 0x80 != 0) {
        return 0;
    }
    let size = ((s[0] as u64) << 21) | ((s[1] as u64) << 14) | ((s[2] as u64) << 7) | s[3] as u64;
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// Bitrate (kbit/s) of the MPEG audio frame header at the start of `h`.
fn frame_bitrate(h: &[u8]) -> Option<u32> {
    if h.len() < 4 || h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (h[1] >> 1) & 0x3; // 1 = III, 2 = II, 3 = I
    let index = (h[2] >> 4) as usize;
    if version == 1 || layer == 0 || index == 0 || index == 15 || (h[2] >> 2) & 0x3 == 3 {
        return None;
    }
    const V1_L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V1_L2: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V2_L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    match (version, layer) {
        (3, 1) => Some(V1_L3[index]),
        (3, 2) => Some(V1_L2[index]),
        (_, 1 | 2) => Some(V2_L23[index]),
        _ => None,
    }
}

fn find_mp3_frame(head: &[u8], from: usize) -> Option<(usize, u32)> {
    (from..head.len().saturating_sub(3)).find_map(|i| frame_bitrate(&head[i..]).map(|b| (i, b)))
}

fn sniff_mp3(head: &[u8], total: Option<u64>) -> Sniffed {
    let start = id3_size(head);
    let mut out = Sniffed {
        audio_start: start,
        byte_seekable: true,
        duration_ms: 0,
    };
    let from = usize::try_from(start).unwrap_or(usize::MAX);
    if let (Some(total), Some((_, kbps))) = (total, find_mp3_frame(head, from.min(head.len()))) {
        if total > start && kbps > 0 {
            out.duration_ms = ((total - start) * 8 / kbps as u64) as i64;
        }
    }
    out
}

fn sniff_wav(head: &[u8], total: Option<u64>) -> Sniffed {
    let mut pos = 12;
    let mut byte_rate = 0u64;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32::from_le_bytes([head[pos + 4], head[pos + 5], head[pos + 6], head[pos + 7]]) as u64;
        let body = pos + 8;
        if id == b"fmt " && body + 12 <= head.len() {
            byte_rate = u32::from_le_bytes([head[body + 8], head[body + 9], head[body + 10], head[body + 11]]) as u64;
        }
        if id == b"data" {
            let data = if size == 0 || size == u32::MAX as u64 {
                total.map(|t| t.saturating_sub(body as u64)).unwrap_or(0)
            } else {
                size
            };
            let duration_ms = (data * 1000).checked_div(byte_rate).unwrap_or(0) as i64;
            return Sniffed {
                duration_ms,
                audio_start: body as u64,
                byte_seekable: false,
            };
        }
        pos = body.saturating_add(size as usize).saturating_add(size as usize & 1);
    }
    Sniffed::default()
}

/// The first stream URL of an `.m3u`/`.pls` playlist body (not HLS).
pub fn playlist_entry(body: &str) -> Option<String> {
    if body.contains("#EXT-X-") {
        return None;
    }
    body.lines().map(str::trim).find_map(|l| {
        let v = match l.split_once('=') {
            Some((k, v)) if k.to_ascii_lowercase().starts_with("file") => v.trim(),
            _ => l,
        };
        (v.starts_with("http://") || v.starts_with("https://")).then(|| v.to_string())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake 128 kbit/s MPEG1 Layer III file of `len` bytes with an ID3 tag.
    pub fn fake_mp3(len: usize) -> Vec<u8> {
        let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        v.extend_from_slice(&[0u8; 10]);
        while v.len() < len {
            v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            v.extend(std::iter::repeat_n(0u8, 413));
        }
        v.truncate(len);
        v
    }

    pub fn fake_wav(seconds: u32) -> Vec<u8> {
        let rate = 48_000u32 * 4;
        let data = rate * seconds;
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&[1, 0, 2, 0]);
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&[4, 0, 16, 0]);
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        v.resize(44 + data as usize, 0);
        v
    }

    #[test]
    fn hints() {
        assert_eq!(hint_for_mime("audio/mpeg; charset=x"), Some("mp3"));
        assert_eq!(hint_for_mime("application/ogg"), Some("ogg"));
        assert_eq!(hint_for_mime("text/html"), None);
        assert_eq!(hint_for_path("/a/b/Song.FLAC"), Some("flac"));
        assert_eq!(hint_for_path("/a/b/x.opus"), Some("ogg"));
        assert_eq!(hint_for_path("/a/b/readme"), None);
    }

    #[test]
    fn mp3_duration_is_estimated_from_the_first_frame() {
        let total = 20 + 16_000 * 10; // 10 s at 128 kbit/s after a 20 byte tag
        let f = fake_mp3(SNIFF_BYTES);
        let s = sniff(&f, Some(total as u64), Some("mp3"));
        assert_eq!(s.audio_start, 20);
        assert_eq!(s.duration_ms, 10_000);
        assert!(s.byte_seekable);
    }

    #[test]
    fn wav_duration_is_read_from_the_header() {
        let f = fake_wav(3);
        let s = sniff(&f[..SNIFF_BYTES], Some(f.len() as u64), None);
        assert_eq!(s.duration_ms, 3000);
        assert_eq!(s.audio_start, 44);
        assert!(!s.byte_seekable);
    }

    #[test]
    fn playlists() {
        assert_eq!(
            playlist_entry("[playlist]\nNumberOfEntries=1\nFile1=http://radio.example/stream\n").as_deref(),
            Some("http://radio.example/stream")
        );
        assert_eq!(
            playlist_entry("#EXTM3U\n#EXTINF:-1,Radio\nhttps://r.example/live.mp3\n").as_deref(),
            Some("https://r.example/live.mp3")
        );
        assert_eq!(playlist_entry("#EXTM3U\n#EXT-X-VERSION:3\nseg.ts\n"), None);
    }
}
