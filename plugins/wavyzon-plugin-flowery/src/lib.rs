//! # wavyzon-plugin-flowery
//!
//! [Flowery](https://flowery.pw) text-to-speech as a Wavyzon source, with
//! the identifiers of LavaSrc's Flowery source, so Lavalink clients' `ftts`
//! search (lavalink-client's `source: "ftts"`) works unchanged:
//!
//! ```text
//! GET /v4/loadtracks?identifier=ftts://Hello%20world%21
//! GET /v4/loadtracks?identifier=ftts://Hello%20world%21?voice=<id or name>&translate=false&silence=500&speed=1.2&audio_format=mp3
//! ```
//!
//! Loading never calls Flowery: the track is built from the identifier
//! (title = the start of the text, author `Flowery TTS`, `uri` = the Flowery
//! request). Playing it streams `GET https://api.flowery.pw/v1/tts?text=…`
//! through the shared HTTP client and the normal decoder, like an `http`
//! track. Flowery needs no API key.
//!
//! The source is `flowery-tts` (LavaSrc's name, stored in tracks) and is also
//! registered as `ftts`, so `/v4/info` lists both names clients look for.
//!
//! On by default (cargo feature `plugin-flowery`). Settings, all optional,
//! from `WAVYZON_PLUGIN_FLOWERY_<KEY>`:
//!
//! | Variable | Default | Meaning |
//! | --- | --- | --- |
//! | `ENABLED` | `true` | `false` registers nothing (no `ftts` source) |
//! | `VOICE` | Flowery's default | Default voice id or name |
//! | `TRANSLATE` | `false` | Translate the text to the voice's language |
//! | `SILENCE` | `0` | Silence after the speech, ms (0–10 000) |
//! | `SPEED` | `1.0` | 0.5–10 |
//! | `AUDIO_FORMAT` | `mp3` | `mp3`, `ogg_vorbis`, `aac`, `wav` or `flac` |
//! | `API_URL` | `https://api.flowery.pw/v1/tts` | Flowery's `/v1/tts` endpoint |
//! | `TIMEOUT_MS` | `30000` | Wait for Flowery to start answering (long texts take a while) |
//! | `PROXY` | none | `http://`, `https://` or `socks5://` proxy |

mod spec;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use url::Url;
use wavyzon_plugin::{
    API_VERSION, Failure, Format, LoadResult, Playable, Plugin, PluginContext, PluginError, PluginManifest, Registry,
    Source, Stream, Track, async_trait,
};
use wavyzon_sources::httpx::{HttpClient, HttpOptions, Req};
use wavyzon_sources::probe::hint_for_mime;

pub use spec::{
    AudioFormat, Defaults, MAX_CHARS, Options, Resolved, SILENCE_MAX_MS, SPEED_MAX, SPEED_MIN, TITLE_CHARS, TtsRequest,
    is_tts,
};

/// Plugin name: `WAVYZON_PLUGINS` entry and `WAVYZON_PLUGIN_FLOWERY_*` settings.
pub const NAME: &str = "flowery";
/// The source, as in LavaSrc: stored in [`Track::source`], `sourceName` on v4.
pub const SOURCE_NAME: &str = "flowery-tts";
/// The name lavalink-client looks for first in `sourceManagers`.
pub const ALIAS_NAME: &str = "ftts";
/// Author of every TTS track.
pub const AUTHOR: &str = "Flowery TTS";
/// Flowery's text-to-speech endpoint.
pub const DEFAULT_API_URL: &str = "https://api.flowery.pw/v1/tts";
/// Upper bound of one TTS answer. 2 048 characters of speech are a few MB;
/// a longer body is cut there rather than read without end.
pub const MAX_AUDIO_BYTES: u64 = 64 << 20;

/// Plugin settings (see the crate docs).
#[derive(Debug, Clone)]
pub struct Settings {
    pub enabled: bool,
    pub api_url: Url,
    pub defaults: Defaults,
    pub timeout: Duration,
    pub proxy: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            api_url: Url::parse(DEFAULT_API_URL).expect("default url parses"),
            defaults: Defaults::default(),
            timeout: Duration::from_secs(30),
            proxy: None,
        }
    }
}

impl Settings {
    /// Read `WAVYZON_PLUGIN_FLOWERY_*`. Flags use the node's three-way rule
    /// (`1/true/yes/on`, `0/false/no/off`, anything else keeps the default);
    /// other bad values stop the node with a message.
    pub fn from_context(ctx: &PluginContext) -> Result<Self, PluginError> {
        let get = |k: &str| ctx.setting(k).map(str::trim).filter(|v| !v.is_empty());
        let flag = |k: &str, d: bool| match get(k).map(str::to_ascii_lowercase).as_deref() {
            Some("1" | "true" | "yes" | "on") => true,
            Some("0" | "false" | "no" | "off") => false,
            _ => d,
        };
        let bad = |k: &str, msg: String| PluginError::Config(format!("WAVYZON_PLUGIN_FLOWERY_{k}: {msg}"));
        let mut s = Self {
            enabled: flag("enabled", true),
            ..Self::default()
        };
        s.defaults.translate = flag("translate", false);
        if let Some(v) = get("voice") {
            s.defaults.voice = Some(spec::check_voice(v).map_err(|e| bad("VOICE", e))?);
        }
        if let Some(v) = get("silence") {
            let n: u32 = v
                .parse()
                .map_err(|_| bad("SILENCE", format!("{v:?} is not a number")))?;
            if n > SILENCE_MAX_MS {
                return Err(bad("SILENCE", format!("{n} is over {SILENCE_MAX_MS} ms")));
            }
            s.defaults.silence_ms = n;
        }
        if let Some(v) = get("speed") {
            let n: f32 = v.parse().map_err(|_| bad("SPEED", format!("{v:?} is not a number")))?;
            if !(SPEED_MIN..=SPEED_MAX).contains(&n) {
                return Err(bad("SPEED", format!("{n} is not between {SPEED_MIN} and {SPEED_MAX}")));
            }
            s.defaults.speed = n;
        }
        if let Some(v) = get("audio_format") {
            match AudioFormat::parse(v) {
                Ok((f, false)) => s.defaults.audio_format = f,
                Ok((_, true)) => {
                    return Err(bad(
                        "AUDIO_FORMAT",
                        format!("{v} cannot be decoded by Wavyzon; use mp3, ogg_vorbis, aac, wav or flac"),
                    ));
                }
                Err(e) => return Err(bad("AUDIO_FORMAT", e)),
            }
        }
        if let Some(v) = get("api_url") {
            let u = Url::parse(v).map_err(|e| bad("API_URL", e.to_string()))?;
            if !matches!(u.scheme(), "http" | "https") {
                return Err(bad("API_URL", "must be an http(s) URL".into()));
            }
            s.api_url = u;
        }
        if let Some(v) = get("timeout_ms") {
            let ms: u64 = v
                .parse()
                .ok()
                .filter(|&ms| ms > 0)
                .ok_or_else(|| bad("TIMEOUT_MS", format!("{v:?} is not a positive number")))?;
            s.timeout = Duration::from_millis(ms);
        }
        s.proxy = get("proxy").map(str::to_string);
        Ok(s)
    }
}

struct Inner {
    settings: Settings,
    http: HttpClient,
}

/// The Flowery TTS source. One instance per advertised name, sharing a client.
#[derive(Clone)]
pub struct FlowerySource {
    name: &'static str,
    /// Only the main instance claims `ftts://` identifiers; the alias only
    /// plays tracks stored under its name.
    claims: bool,
    inner: Arc<Inner>,
}

impl FlowerySource {
    /// The main source (`flowery-tts`). `user_agent` is sent to Flowery.
    pub fn new(settings: Settings, user_agent: &str) -> Result<Self, PluginError> {
        let t = settings.timeout;
        let opts = HttpOptions {
            proxy: settings.proxy.clone(),
            connect_timeout: t.min(Duration::from_secs(10)),
            request_timeout: t,
            // Flowery synthesises the whole text before the first byte.
            header_timeout: t,
            user_agent: user_agent.to_string(),
            // The endpoint is the operator's setting, never a client's.
            block_private: false,
            ..HttpOptions::default()
        };
        let http = HttpClient::new(SOURCE_NAME, opts).map_err(|e| PluginError::Config(e.to_string()))?;
        Ok(Self {
            name: SOURCE_NAME,
            claims: true,
            inner: Arc::new(Inner { settings, http }),
        })
    }

    /// The same source under the `ftts` name, for `sourceManagers` and for
    /// tracks a client stored with that name.
    pub fn alias(&self) -> Self {
        Self {
            name: ALIAS_NAME,
            claims: false,
            inner: self.inner.clone(),
        }
    }

    fn invalid(&self, msg: String) -> Failure {
        Failure::common(self.name, "invalid Flowery TTS request", Some(msg))
    }

    /// The track for a parsed request.
    pub fn to_track(&self, r: &TtsRequest) -> Track {
        let s = &self.inner.settings;
        Track {
            source: self.name.into(),
            identifier: r.identifier(),
            title: r.title(),
            author: AUTHOR.into(),
            duration_ms: 0,
            uri: r.resolve(&s.defaults).url(&s.api_url).to_string(),
            is_stream: false,
            ..Default::default()
        }
    }

    fn request(&self, identifier: &str) -> Result<TtsRequest, Failure> {
        match TtsRequest::parse(identifier) {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(self.invalid("no text to speak".into())),
            Err(e) => Err(self.invalid(e)),
        }
    }
}

#[async_trait]
impl Source for FlowerySource {
    fn name(&self) -> &str {
        self.name
    }

    fn matches_url(&self, url: &str) -> bool {
        self.claims && is_tts(url)
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        Ok(match TtsRequest::parse(url) {
            Ok(Some(r)) => LoadResult::single(self.to_track(&r)),
            Ok(None) => LoadResult::empty(),
            Err(e) => LoadResult::error(self.invalid(e)),
        })
    }

    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        Ok(self.to_track(&self.request(identifier)?))
    }

    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for FlowerySource {
    /// Streams Flowery's answer from the start; a seek is done by the
    /// player skipping forward (Flowery has no ranges, and TTS is short).
    async fn open(&self, track: &Track, _start_ms: i64) -> Result<Stream, Failure> {
        let s = &self.inner.settings;
        let r = self.request(&track.identifier)?.resolve(&s.defaults);
        let url = r.url(&s.api_url);
        let req = Req::get(url.as_str())
            .map_err(|e| e.to_failure(self.name))?
            .header("Accept", "audio/*");
        let res = self
            .inner
            .http
            .stream(&req)
            .await
            .map_err(|e| e.to_failure(self.name))?;
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("application/json") || content_type.starts_with("text/") {
            return Err(Failure::common(
                self.name,
                "Flowery returned no audio",
                Some(format!("content type {content_type:?}")),
            ));
        }
        let hint = hint_for_mime(&content_type).unwrap_or(r.audio_format.hint());
        // Never log the text: it is what a user typed.
        tracing::debug!(
            source = self.name,
            chars = r.text.chars().count(),
            voice = r.voice.as_deref().unwrap_or("default"),
            format = %r.audio_format,
            "opened Flowery TTS stream"
        );
        Ok(Stream {
            body: Box::new(wavyzon_sources::response_body(res).take(MAX_AUDIO_BYTES)),
            format: Format::Encoded {
                hint: Some(hint.to_string()),
            },
            start_sample: 0,
            live: false,
        })
    }
}

/// The plugin: registers the source under both names unless disabled.
#[derive(Default, Clone)]
pub struct FloweryPlugin;

#[async_trait]
impl Plugin for FloweryPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            description: "Flowery text-to-speech source (ftts://, LavaSrc-compatible)",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let settings = Settings::from_context(ctx)?;
        if !settings.enabled {
            tracing::info!("flowery plugin disabled (WAVYZON_PLUGIN_FLOWERY_ENABLED=false), no ftts source");
            return Ok(());
        }
        let ua = format!("Wavyzon/{} (+https://gitlab.com/ihrz/wavyzon)", ctx.node_version);
        let main = FlowerySource::new(settings, &ua)?;
        reg.source(Arc::new(main.alias())).source(Arc::new(main));
        Ok(())
    }
}

/// Entry point used by the node.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(FloweryPlugin)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use wavyzon_plugin::axum::Router;
    use wavyzon_plugin::axum::extract::{Query, State};
    use wavyzon_plugin::axum::http::{HeaderMap, StatusCode, header};
    use wavyzon_plugin::axum::response::IntoResponse;
    use wavyzon_plugin::axum::routing::get;
    use wavyzon_plugin::{LoadType, Severity};

    use super::*;

    fn ctx(pairs: &[(&str, &str)]) -> PluginContext {
        PluginContext {
            settings: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            data_dir: std::env::temp_dir().join("wavyzon-flowery-test"),
            node_version: "test",
        }
    }

    fn source(settings: Settings) -> FlowerySource {
        FlowerySource::new(settings, "wavyzon-test").unwrap()
    }

    #[tokio::test]
    async fn registers_both_names_unless_disabled() {
        let mut reg = Registry::new();
        reg.begin(NAME);
        FloweryPlugin.register(&ctx(&[]), &mut reg).await.unwrap();
        let names: Vec<&str> = reg.sources().iter().map(|s| s.name()).collect();
        assert_eq!(names, [ALIAS_NAME, SOURCE_NAME]);
        // only the main one claims identifiers, so tracks say flowery-tts
        assert!(reg.sources()[1].matches_url("ftts://hi") && !reg.sources()[0].matches_url("ftts://hi"));
        assert!(reg.sources()[0].playable().is_some());

        for off in ["false", "0", "off"] {
            let mut reg = Registry::new();
            FloweryPlugin
                .register(&ctx(&[("enabled", off)]), &mut reg)
                .await
                .unwrap();
            assert!(reg.sources().is_empty(), "{off}");
        }
        let mut reg = Registry::new();
        FloweryPlugin
            .register(&ctx(&[("enabled", "typo")]), &mut reg)
            .await
            .unwrap();
        assert_eq!(reg.sources().len(), 2, "a typo keeps the default (on)");
    }

    #[test]
    fn settings_have_defaults_and_refuse_bad_values() {
        let s = Settings::from_context(&ctx(&[])).unwrap();
        assert!(s.enabled);
        assert_eq!(s.api_url.as_str(), DEFAULT_API_URL);
        assert_eq!(s.defaults, Defaults::default());
        assert_eq!(s.timeout, Duration::from_secs(30));

        let s = Settings::from_context(&ctx(&[
            ("voice", "Salli"),
            ("translate", "yes"),
            ("silence", "200"),
            ("speed", "1.5"),
            ("audio_format", "ogg_vorbis"),
            ("timeout_ms", "5000"),
        ]))
        .unwrap();
        assert_eq!(s.defaults.voice.as_deref(), Some("Salli"));
        assert!(s.defaults.translate);
        assert_eq!((s.defaults.silence_ms, s.defaults.speed), (200, 1.5));
        assert_eq!(s.defaults.audio_format, AudioFormat::OggVorbis);
        assert_eq!(s.timeout, Duration::from_millis(5000));

        for bad in [
            ("silence", "20000"),
            ("silence", "-1"),
            ("speed", "11"),
            ("speed", "x"),
            ("audio_format", "ogg_opus"),
            ("audio_format", "midi"),
            ("api_url", "ftp://x"),
            ("api_url", "not a url"),
            ("timeout_ms", "0"),
        ] {
            let err = Settings::from_context(&ctx(&[bad])).unwrap_err();
            assert!(matches!(err, PluginError::Config(_)), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn load_results_have_the_lavalink_shape() {
        let src = source(Settings::default());
        let res = src
            .load_url("ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23")
            .await
            .unwrap();
        assert_eq!(res.kind, LoadType::Track);
        let t = &res.tracks[0];
        assert_eq!(t.source, SOURCE_NAME);
        assert_eq!(t.title, "Hello world!");
        assert_eq!(t.author, AUTHOR);
        assert_eq!(t.duration_ms, 0);
        assert!(!t.is_stream);
        assert_eq!(
            t.identifier,
            "ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23"
        );
        assert_eq!(
            t.uri,
            "https://api.flowery.pw/v1/tts?text=Hello+world%21&voice=fa3ea565-121f-5efd-b4e9-59895c77df23&translate=false&silence=0&speed=1&audio_format=mp3"
        );
        // the encoded track is self-sufficient: another node decodes it to the same request
        let back = Track::decode(&t.encode()).unwrap();
        assert_eq!(src.track(&back.identifier).await.unwrap(), *t);

        assert_eq!(src.load_url("ftts://").await.unwrap().kind, LoadType::Empty);
        let err = src.load_url("ftts://x?speed=fast").await.unwrap();
        assert_eq!(err.kind, LoadType::Error);
        let f = err.error.unwrap();
        assert_eq!((f.severity, f.source.as_str()), (Severity::Common, SOURCE_NAME));
        assert!(f.cause.contains("speed"), "{f:?}");
        let long = format!("ftts://{}", "a".repeat(MAX_CHARS + 1));
        assert_eq!(src.load_url(&long).await.unwrap().kind, LoadType::Error);
        assert!(src.track("ftts://").await.is_err());
    }

    type Seen = Arc<Mutex<Vec<(HashMap<String, String>, String)>>>;

    async fn fake_flowery(seen: Seen) -> String {
        async fn tts(
            State(seen): State<Seen>,
            h: HeaderMap,
            Query(q): Query<HashMap<String, String>>,
        ) -> impl IntoResponse {
            let ua = h
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            seen.lock().unwrap().push((q.clone(), ua));
            match q.get("text").map(String::as_str) {
                Some("limit") => (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::CONTENT_TYPE, "application/json")],
                    b"{}".to_vec(),
                ),
                Some("json") => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    b"{}".to_vec(),
                ),
                Some("bad") => (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    b"{\"error\":\"x\"}".to_vec(),
                ),
                _ => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "audio/mpeg")],
                    b"ID3fake-mp3-bytes".to_vec(),
                ),
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1/tts", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/tts", get(tts)).with_state(seen);
        tokio::spawn(async move {
            let _ = wavyzon_plugin::axum::serve(listener, app).await;
        });
        base
    }

    #[tokio::test]
    async fn plays_the_flowery_answer_through_the_decoder_path() {
        let seen: Seen = Default::default();
        let base = fake_flowery(seen.clone()).await;
        let src = source(Settings {
            api_url: Url::parse(&base).unwrap(),
            defaults: Defaults {
                voice: Some("Salli".into()),
                ..Defaults::default()
            },
            ..Settings::default()
        });
        let t = src
            .track("ftts://Bonjour%20%C3%A0%20tous?speed=2&audio_format=wav")
            .await
            .unwrap();
        let mut s = src.playable().unwrap().open(&t, 1_500).await.unwrap();
        assert_eq!(
            s.format,
            Format::Encoded {
                hint: Some("mp3".into())
            },
            "the Content-Type wins"
        );
        assert_eq!((s.start_sample, s.live), (0, false));
        let mut body = Vec::new();
        s.body.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"ID3fake-mp3-bytes");
        let (q, ua) = seen.lock().unwrap()[0].clone();
        assert_eq!(q["text"], "Bonjour à tous");
        assert_eq!(q["voice"], "Salli");
        assert_eq!((q["speed"].as_str(), q["audio_format"].as_str()), ("2", "wav"));
        assert_eq!((q["silence"].as_str(), q["translate"].as_str()), ("0", "false"));
        assert_eq!(ua, "wavyzon-test");

        // the alias plays tracks stored under its name
        let alias = src.alias();
        let mut t2 = t.clone();
        t2.source = ALIAS_NAME.into();
        assert!(alias.playable().unwrap().open(&t2, 0).await.is_ok());

        let fail = |text: &str| {
            let src = src.clone();
            let text = text.to_string();
            async move {
                let t = src.track(&format!("ftts://{text}")).await.unwrap();
                src.playable().unwrap().open(&t, 0).await.unwrap_err()
            }
        };
        assert_eq!(fail("limit").await.severity, Severity::Suspicious);
        assert_eq!(fail("bad").await.severity, Severity::Common);
        assert_eq!(fail("json").await.message, "Flowery returned no audio");
    }

    #[tokio::test]
    async fn an_unreachable_flowery_is_a_common_failure() {
        // a port nothing listens on
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/tts", l.local_addr().unwrap());
        drop(l);
        let src = source(Settings {
            api_url: Url::parse(&url).unwrap(),
            timeout: Duration::from_secs(2),
            ..Settings::default()
        });
        let t = src.track("ftts://hi").await.unwrap();
        let f = src.playable().unwrap().open(&t, 0).await.unwrap_err();
        assert_eq!(f.severity, Severity::Common);
        assert!(!f.cause.contains("text=hi"), "errors never carry the query: {f:?}");
    }
}
