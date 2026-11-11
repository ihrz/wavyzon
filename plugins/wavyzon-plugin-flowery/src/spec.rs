//! `ftts://` identifiers: parsing, bounds, the canonical form stored in
//! tracks, and the Flowery request URL they become.
//!
//! The format is LavaSrc's (`FloweryTTSSourceManager`), which lavalink-client
//! builds as `ftts://` + `encodeURIComponent(text)` + an optional query:
//!
//! ```text
//! ftts://Hello%20world%21
//! ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23&speed=1.2&silence=500
//! ```
//!
//! Parameters, as in LavaSrc and Flowery's `GET /v1/tts`: `voice` (id or
//! name), `translate` (bool), `silence` (ms, clamped to 0–10 000), `speed`
//! (clamped to 0.5–10) and `audio_format`.

use std::fmt;

use url::Url;

/// Flowery refuses longer texts; LavaSrc rejects them before asking.
pub const MAX_CHARS: usize = 2048;
/// `silence` bound in milliseconds (LavaSrc's `SILENCE_MAX`).
pub const SILENCE_MAX_MS: u32 = 10_000;
/// `speed` bounds (LavaSrc's `SPEED_MIN`/`SPEED_MAX`).
pub const SPEED_MIN: f32 = 0.5;
pub const SPEED_MAX: f32 = 10.0;
/// Track titles show the start of the text.
pub const TITLE_CHARS: usize = 100;
/// Longest voice id or name accepted (ids are 36-character UUIDs).
pub const MAX_VOICE_LEN: usize = 128;

const KEYS: [&str; 5] = ["voice", "translate", "silence", "speed", "audio_format"];

/// The audio formats Flowery serves that Wavyzon's decoder (Symphonia) reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    OggVorbis,
    Aac,
    Wav,
    Flac,
}

impl AudioFormat {
    /// Parse a Flowery `audio_format`. `ogg_opus` and `pcm` are valid for
    /// Flowery but Wavyzon cannot decode them (no Opus decoder, headerless
    /// PCM of unknown rate): they fall back to MP3, flagged by the `bool`, so
    /// identifiers copied from LavaSrc's docs still play.
    pub fn parse(s: &str) -> Result<(Self, bool), String> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "mp3" => (Self::Mp3, false),
            "ogg_vorbis" => (Self::OggVorbis, false),
            "aac" => (Self::Aac, false),
            "wav" => (Self::Wav, false),
            "flac" => (Self::Flac, false),
            "ogg_opus" | "pcm" => (Self::Mp3, true),
            other => {
                return Err(format!(
                    "audio_format {other:?} is not supported (mp3, ogg_vorbis, aac, wav or flac)"
                ));
            }
        })
    }

    /// The name Flowery expects.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::OggVorbis => "ogg_vorbis",
            Self::Aac => "aac",
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }

    /// Decoder hint when the response has no usable `Content-Type`.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::OggVorbis => "ogg",
            Self::Aac => "aac",
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Options given in an identifier; `None` falls back to the node's defaults.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Options {
    pub voice: Option<String>,
    pub translate: Option<bool>,
    pub silence_ms: Option<u32>,
    pub speed: Option<f32>,
    pub audio_format: Option<AudioFormat>,
}

/// The node-wide defaults (`WAVYZON_PLUGIN_FLOWERY_*`), Flowery's own when unset.
#[derive(Debug, Clone, PartialEq)]
pub struct Defaults {
    /// `None` = Flowery's default voice.
    pub voice: Option<String>,
    pub translate: bool,
    pub silence_ms: u32,
    pub speed: f32,
    pub audio_format: AudioFormat,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            voice: None,
            translate: false,
            silence_ms: 0,
            speed: 1.0,
            audio_format: AudioFormat::Mp3,
        }
    }
}

/// A parsed `ftts://` identifier.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsRequest {
    /// The decoded text, trimmed, at most [`MAX_CHARS`] characters.
    pub text: String,
    pub options: Options,
}

/// Every option resolved against the defaults: what is sent to Flowery.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub text: String,
    pub voice: Option<String>,
    pub translate: bool,
    pub silence_ms: u32,
    pub speed: f32,
    pub audio_format: AudioFormat,
}

/// The identifier without its `ftts://` (or `ftts:`) prefix, case-insensitive.
pub fn strip_scheme(identifier: &str) -> Option<&str> {
    let id = identifier.trim();
    let head = id.get(..5)?;
    if !head.eq_ignore_ascii_case("ftts:") {
        return None;
    }
    let rest = &id[5..];
    Some(rest.strip_prefix("//").unwrap_or(rest))
}

/// Whether `identifier` is a Flowery TTS identifier.
pub fn is_tts(identifier: &str) -> bool {
    strip_scheme(identifier).is_some()
}

/// Decode `%XX` escapes (UTF-8, lossy). `+` stays a plus sign, as
/// `encodeURIComponent` writes spaces as `%20`; malformed escapes are kept.
pub fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode like `encodeURIComponent` (spaces as `%20`).
pub fn encode_component(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn query_key(pair: &str) -> String {
    percent_decode(pair.split('=').next().unwrap_or(""))
        .trim()
        .to_ascii_lowercase()
}

/// Whether `q` (after a `?`) is an option query: non-empty, and every pair
/// names a known option. Otherwise the `?` belongs to the text ("How are
/// you?"), which a client may have left unencoded.
fn is_option_query(q: &str) -> bool {
    let mut pairs = q.split('&').filter(|p| !p.is_empty()).peekable();
    pairs.peek().is_some() && pairs.all(|p| p.contains('=') && KEYS.contains(&query_key(p).as_str()))
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Validate a voice id or name.
pub fn check_voice(v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.chars().count() > MAX_VOICE_LEN {
        return Err(format!("voice is longer than {MAX_VOICE_LEN} characters"));
    }
    if v.chars().any(char::is_control) {
        return Err("voice contains control characters".into());
    }
    Ok(v.to_string())
}

/// Parse `silence` (milliseconds), clamped to `0..=SILENCE_MAX_MS`.
pub fn parse_silence(v: &str) -> Result<u32, String> {
    let n: f64 = v
        .trim()
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite())
        .ok_or_else(|| format!("silence {v:?} is not a number of milliseconds"))?;
    Ok(n.clamp(0.0, SILENCE_MAX_MS as f64) as u32)
}

/// Parse `speed`, clamped to `SPEED_MIN..=SPEED_MAX`.
pub fn parse_speed(v: &str) -> Result<f32, String> {
    let n: f32 = v
        .trim()
        .parse()
        .ok()
        .filter(|n: &f32| n.is_finite())
        .ok_or_else(|| format!("speed {v:?} is not a number"))?;
    Ok(n.clamp(SPEED_MIN, SPEED_MAX))
}

impl TtsRequest {
    /// Parse an identifier. `Ok(None)` when it is a TTS identifier without
    /// text (an `empty` load result); `Err` for bad options or a text over
    /// [`MAX_CHARS`] (an `error` load result).
    pub fn parse(identifier: &str) -> Result<Option<Self>, String> {
        let rest = strip_scheme(identifier).ok_or("not an ftts:// identifier")?;
        let (raw_text, query) = rest
            .match_indices('?')
            .map(|(i, _)| i)
            .find(|&i| is_option_query(&rest[i + 1..]))
            .map_or((rest, ""), |i| (&rest[..i], &rest[i + 1..]));
        let text = percent_decode(raw_text).trim().to_string();
        if text.is_empty() {
            return Ok(None);
        }
        let chars = text.chars().count();
        if chars > MAX_CHARS {
            return Err(format!(
                "text is {chars} characters long, Flowery accepts at most {MAX_CHARS}"
            ));
        }
        let mut o = Options::default();
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if v.trim().is_empty() {
                continue;
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "voice" => o.voice = Some(check_voice(&v)?),
                "translate" => {
                    o.translate = Some(parse_bool(&v).ok_or_else(|| format!("translate {v:?} is not a boolean"))?)
                }
                "silence" => o.silence_ms = Some(parse_silence(&v)?),
                "speed" => o.speed = Some(parse_speed(&v)?),
                "audio_format" => o.audio_format = Some(AudioFormat::parse(&v)?.0),
                _ => {}
            }
        }
        Ok(Some(Self { text, options: o }))
    }

    /// The canonical identifier: the text encoded like `encodeURIComponent`,
    /// then the options that were given, in a fixed order. Re-parses to the
    /// same request on any node (defaults are applied when playing).
    pub fn identifier(&self) -> String {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        let o = &self.options;
        if let Some(v) = &o.voice {
            q.append_pair("voice", v);
        }
        if let Some(v) = o.translate {
            q.append_pair("translate", if v { "true" } else { "false" });
        }
        if let Some(v) = o.silence_ms {
            q.append_pair("silence", &v.to_string());
        }
        if let Some(v) = o.speed {
            q.append_pair("speed", &v.to_string());
        }
        if let Some(v) = o.audio_format {
            q.append_pair("audio_format", v.name());
        }
        let q = q.finish();
        let mut id = format!("ftts://{}", encode_component(&self.text));
        if !q.is_empty() {
            id.push('?');
            id.push_str(&q);
        }
        id
    }

    /// The start of the text, for the track title.
    pub fn title(&self) -> String {
        if self.text.chars().count() <= TITLE_CHARS {
            return self.text.clone();
        }
        let mut t: String = self.text.chars().take(TITLE_CHARS - 1).collect();
        t.truncate(t.trim_end().len());
        t.push('…');
        t
    }

    /// Fill the missing options from `d`.
    pub fn resolve(&self, d: &Defaults) -> Resolved {
        let o = &self.options;
        Resolved {
            text: self.text.clone(),
            voice: o.voice.clone().or_else(|| d.voice.clone()).filter(|v| !v.is_empty()),
            translate: o.translate.unwrap_or(d.translate),
            silence_ms: o.silence_ms.unwrap_or(d.silence_ms),
            speed: o.speed.unwrap_or(d.speed),
            audio_format: o.audio_format.unwrap_or(d.audio_format),
        }
    }
}

impl Resolved {
    /// The Flowery request: `<base>?text=…&voice=…&translate=…&silence=…&speed=…&audio_format=…`
    /// (no `voice` when none is set: Flowery picks its default).
    pub fn url(&self, base: &Url) -> Url {
        let mut u = base.clone();
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("text", &self.text);
            if let Some(v) = &self.voice {
                q.append_pair("voice", v);
            }
            q.append_pair("translate", if self.translate { "true" } else { "false" })
                .append_pair("silence", &self.silence_ms.to_string())
                .append_pair("speed", &self.speed.to_string())
                .append_pair("audio_format", self.audio_format.name());
        }
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(id: &str) -> TtsRequest {
        TtsRequest::parse(id).unwrap().unwrap()
    }

    #[test]
    fn parses_what_lavalink_client_sends() {
        let r = parse("ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23");
        assert_eq!(r.text, "Hello world!");
        assert_eq!(r.options.voice.as_deref(), Some("fa3ea565-121f-5efd-b4e9-59895c77df23"));
        assert_eq!(r.options.speed, None);

        let r = parse("ftts://Bonjour%20%C3%A0%20tous");
        assert_eq!(r.text, "Bonjour à tous");
        assert_eq!(r.options, Options::default());
    }

    #[test]
    fn accepts_prefix_variants_and_raw_text() {
        assert_eq!(parse("FTTS://hi").text, "hi");
        assert_eq!(parse("ftts:hi there").text, "hi there");
        assert_eq!(parse("  ftts://a+b  ").text, "a+b", "+ is not a space");
        assert_eq!(parse("ftts://100%25 sure %zz").text, "100% sure %zz");
        assert!(!is_tts("https://example.com/ftts://x"));
        assert!(!is_tts("ftt"));
        assert!(TtsRequest::parse("scsearch:x").is_err());
    }

    #[test]
    fn question_marks_belong_to_the_text_unless_options_follow() {
        assert_eq!(parse("ftts://How are you?").text, "How are you?");
        let r = parse("ftts://Why? Because.?speed=2");
        assert_eq!(r.text, "Why? Because.");
        assert_eq!(r.options.speed, Some(2.0));
        assert_eq!(parse("ftts://what?foo=bar").text, "what?foo=bar");
        let r = parse("ftts://x?VOICE=Salli&");
        assert_eq!(r.options.voice.as_deref(), Some("Salli"));
    }

    #[test]
    fn options_are_bounded() {
        let r = parse("ftts://x?silence=99999&speed=20&translate=False&audio_format=FLAC");
        assert_eq!(r.options.silence_ms, Some(SILENCE_MAX_MS));
        assert_eq!(r.options.speed, Some(SPEED_MAX));
        assert_eq!(r.options.translate, Some(false));
        assert_eq!(r.options.audio_format, Some(AudioFormat::Flac));
        let r = parse("ftts://x?silence=-5&speed=0.1&translate=1");
        assert_eq!((r.options.silence_ms, r.options.speed), (Some(0), Some(SPEED_MIN)));
        assert_eq!(r.options.translate, Some(true));
        // empty values keep the defaults
        assert_eq!(parse("ftts://x?voice=&speed=").options, Options::default());
    }

    #[test]
    fn opus_and_pcm_fall_back_to_mp3() {
        assert_eq!(AudioFormat::parse("ogg_opus").unwrap(), (AudioFormat::Mp3, true));
        assert_eq!(AudioFormat::parse("pcm").unwrap(), (AudioFormat::Mp3, true));
        assert_eq!(
            parse("ftts://x?audio_format=ogg_opus").options.audio_format,
            Some(AudioFormat::Mp3)
        );
    }

    #[test]
    fn bad_input_is_an_error_and_no_text_is_empty() {
        for bad in [
            "ftts://x?speed=fast",
            "ftts://x?speed=NaN",
            "ftts://x?silence=soon",
            "ftts://x?translate=maybe",
            "ftts://x?audio_format=opus",
            "ftts://x?voice=a%0Ab",
        ] {
            assert!(TtsRequest::parse(bad).is_err(), "{bad}");
        }
        let long = format!("ftts://x?voice={}", "v".repeat(MAX_VOICE_LEN + 1));
        assert!(TtsRequest::parse(&long).is_err());
        for empty in ["ftts://", "ftts:", "ftts://%20%20", "ftts://?voice=x"] {
            assert_eq!(TtsRequest::parse(empty), Ok(None), "{empty}");
        }
    }

    #[test]
    fn text_length_is_limited_in_characters() {
        let ok = format!("ftts://{}", encode_component(&"é".repeat(MAX_CHARS)));
        assert_eq!(parse(&ok).text.chars().count(), MAX_CHARS);
        let over = format!("ftts://{}", "a".repeat(MAX_CHARS + 1));
        let err = TtsRequest::parse(&over).unwrap_err();
        assert!(err.contains("2049"), "{err}");
    }

    #[test]
    fn canonical_identifier_round_trips() {
        let r = parse("ftts://Ça va? 100%25 & +1?audio_format=wav&speed=1.5&voice=Salli&silence=250&translate=yes");
        assert_eq!(r.text, "Ça va? 100% & +1");
        let id = r.identifier();
        assert_eq!(
            id,
            "ftts://%C3%87a%20va%3F%20100%25%20%26%20%2B1?voice=Salli&translate=true&silence=250&speed=1.5&audio_format=wav"
        );
        assert_eq!(parse(&id), r);
        assert_eq!(parse("ftts://plain").identifier(), "ftts://plain");
    }

    #[test]
    fn titles_are_truncated() {
        assert_eq!(parse("ftts://short").title(), "short");
        let t = parse(&format!("ftts://{}", "word ".repeat(100))).title();
        assert!(t.chars().count() <= TITLE_CHARS && t.ends_with('…'), "{t}");
        assert!(!t.contains(" …"));
    }

    #[test]
    fn resolves_against_defaults_into_the_flowery_url() {
        let base = Url::parse("https://api.flowery.pw/v1/tts").unwrap();
        let d = Defaults {
            voice: Some("Salli".into()),
            speed: 1.25,
            ..Defaults::default()
        };
        let url = parse("ftts://Hello%20world%21?silence=500").resolve(&d).url(&base);
        assert_eq!(
            url.as_str(),
            "https://api.flowery.pw/v1/tts?text=Hello+world%21&voice=Salli&translate=false&silence=500&speed=1.25&audio_format=mp3"
        );
        let url = parse("ftts://a?voice=X").resolve(&Defaults::default()).url(&base);
        assert!(url.as_str().contains("voice=X"));
        let url = parse("ftts://a").resolve(&Defaults::default()).url(&base);
        assert!(!url.as_str().contains("voice="), "no voice: Flowery's default");
    }
}
