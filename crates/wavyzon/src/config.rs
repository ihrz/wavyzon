//! Node configuration from environment variables (port of Cadence's
//! `internal/config`, `CADENCE_*` renamed to `WAVYZON_*`).
//!
//! `docs/src/configuration.md` and `.env.example` document every variable.

use std::path::PathBuf;
use std::time::Duration;

use wavyzon_audio::codec::{MAX_BITRATE, MIN_BITRATE};
use wavyzon_sources::SourcesConfig;

/// Lavalink's default password, refused on purpose.
pub const INSECURE_PASSWORD: &str = "youshallnotpass";

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

/// Everything the node reads from its environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub address: String,
    pub password: String,
    pub data_dir: PathBuf,
    pub buffer_ms: i64,
    pub playout_delay: Duration,
    pub pause_fade: Duration,
    pub bitrate: i32,
    pub resume_timeout: Duration,
    pub update_interval: Duration,
    pub analysis_workers: usize,
    pub analysis_queue: usize,
    pub log_level: String,
    pub log_format: LogFormat,
    pub normalize: bool,
    pub normalize_lufs: i64,
    pub state_file: Option<PathBuf>,
    pub cache_peers: Vec<String>,
    pub cache_secret: String,
    /// `WAVYZON_PLUGINS`; empty means every compiled-in plugin.
    pub plugins: Vec<String>,
    /// `WAVYZON_ALLOW_NO_VOICE`: start even when this build cannot do voice.
    pub allow_no_voice: bool,
    pub sources: SourcesConfig,
}

/// Why the configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("WAVYZON_PASSWORD must be set to a non-default value")]
    Password,
    #[error("{key}: {value:?} is not a number")]
    NotANumber { key: &'static str, value: String },
    #[error("WAVYZON_LOG_FORMAT: {0:?} is neither json nor pretty")]
    LogFormat(String),
}

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn s(get: Lookup, key: &str) -> Option<String> {
    get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn str_or(get: Lookup, key: &str, fallback: &str) -> String {
    s(get, key).unwrap_or_else(|| fallback.to_string())
}

fn num(get: Lookup, key: &'static str, fallback: i64) -> Result<i64, ConfigError> {
    match s(get, key) {
        None => Ok(fallback),
        Some(v) => v.parse().map_err(|_| ConfigError::NotANumber { key, value: v }),
    }
}

/// `1/true/yes/on` and `0/false/no/off`; anything else is the default.
pub fn flag(get: Lookup, key: &str, fallback: bool) -> bool {
    match s(get, key).map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => fallback,
    }
}

fn list(get: Lookup, key: &str) -> Vec<String> {
    s(get, key)
        .unwrap_or_default()
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

impl Config {
    /// Read the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::load(&|k| std::env::var(k).ok())
    }

    /// Read any `name → value` lookup (tests).
    pub fn load(get: Lookup) -> Result<Self, ConfigError> {
        let password = str_or(get, "WAVYZON_PASSWORD", "");
        if password.is_empty() || password == INSECURE_PASSWORD {
            return Err(ConfigError::Password);
        }
        let log_format = match s(get, "WAVYZON_LOG_FORMAT").map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("json") => LogFormat::Json,
            Some("pretty" | "text") => LogFormat::Pretty,
            Some(other) => return Err(ConfigError::LogFormat(other.to_string())),
        };
        let ms = |v: i64| Duration::from_millis(v.max(0) as u64);
        // Numbers the sources crate also reads: validate them here so a typo
        // stops the node instead of silently using the default.
        for key in [
            "WAVYZON_AUDIO_CACHE_GB",
            "WAVYZON_HTTP_MAX_REDIRECTS",
            "WAVYZON_HTTP_TIMEOUT_MS",
        ] {
            num(get, key, 0)?;
        }
        let c = Config {
            address: str_or(get, "WAVYZON_ADDRESS", "0.0.0.0:2333"),
            password,
            data_dir: PathBuf::from(str_or(get, "WAVYZON_DATA_DIR", "./data")),
            buffer_ms: num(get, "WAVYZON_BUFFER_MS", 10_000)?.clamp(1_000, 60_000),
            playout_delay: ms(num(get, "WAVYZON_PLAYOUT_DELAY_MS", 0)?),
            pause_fade: ms(num(get, "WAVYZON_PAUSE_FADE_MS", 0)?.clamp(0, 2_000)),
            bitrate: num(get, "WAVYZON_BITRATE", 96_000)?.clamp(MIN_BITRATE as i64, MAX_BITRATE as i64) as i32,
            resume_timeout: Duration::from_secs(num(get, "WAVYZON_RESUME_TIMEOUT_SECONDS", 300)?.max(1) as u64),
            update_interval: ms(num(get, "WAVYZON_UPDATE_INTERVAL_MS", 1_000)?.max(250)),
            analysis_workers: num(get, "WAVYZON_ANALYSIS_WORKERS", 2)?.clamp(1, 64) as usize,
            analysis_queue: num(get, "WAVYZON_ANALYSIS_QUEUE", 32)?.clamp(1, 100_000) as usize,
            log_level: str_or(get, "WAVYZON_LOG_LEVEL", "info").to_ascii_lowercase(),
            log_format,
            normalize: flag(get, "WAVYZON_NORMALIZE", true),
            normalize_lufs: num(get, "WAVYZON_NORMALIZE_LUFS", -14)?.clamp(-30, -5),
            state_file: s(get, "WAVYZON_STATE_FILE").map(PathBuf::from),
            cache_peers: list(get, "WAVYZON_CACHE_PEERS"),
            cache_secret: str_or(get, "WAVYZON_CACHE_SECRET", ""),
            plugins: list(get, "WAVYZON_PLUGINS")
                .into_iter()
                .map(|p| p.to_ascii_lowercase())
                .collect(),
            allow_no_voice: flag(get, "WAVYZON_ALLOW_NO_VOICE", false),
            sources: SourcesConfig::from_lookup(get),
        };
        Ok(c)
    }

    /// Decoded audio buffered per deck, in 20 ms frames.
    pub fn buffer_frames(&self) -> usize {
        (self.buffer_ms / wavyzon_audio::FRAME_MS) as usize
    }

    /// Whether the peer cache can run (peers, a secret and a disk cache).
    pub fn peer_cache_enabled(&self) -> bool {
        !self.cache_peers.is_empty() && !self.cache_secret.is_empty() && self.sources.audio_cache_dir.is_some()
    }

    /// Valid but probably unintended settings, logged at startup.
    pub fn warnings(&self) -> Vec<String> {
        // The sources crate logs its own warnings when it is built.
        let mut out = Vec::new();
        if !self.cache_peers.is_empty() && !self.peer_cache_enabled() {
            out.push(
                "WAVYZON_CACHE_PEERS is set but the peer cache is off: it also needs WAVYZON_CACHE_SECRET and WAVYZON_AUDIO_CACHE_DIR"
                    .into(),
            );
        }
        let public = !(self.address.starts_with("127.")
            || self.address.starts_with("localhost")
            || self.address.starts_with("[::1]"));
        if self.sources.http.enabled && !self.sources.http.block_private && public {
            out.push(
                "WAVYZON_HTTP_ALLOW_PRIVATE=true on a public address: any client with the password can reach your private network"
                    .into(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::load(&move |k| map.get(k).cloned())
    }

    #[test]
    fn password_is_required() {
        assert_eq!(load(&[]).unwrap_err(), ConfigError::Password);
        assert_eq!(load(&[("WAVYZON_PASSWORD", "  ")]).unwrap_err(), ConfigError::Password);
        assert_eq!(
            load(&[("WAVYZON_PASSWORD", "youshallnotpass")]).unwrap_err(),
            ConfigError::Password
        );
    }

    #[test]
    fn defaults() {
        let c = load(&[("WAVYZON_PASSWORD", "s3cret")]).unwrap();
        assert_eq!(c.address, "0.0.0.0:2333");
        assert_eq!(c.buffer_ms, 10_000);
        assert_eq!(c.buffer_frames(), 500);
        assert_eq!(c.bitrate, 96_000);
        assert_eq!(c.resume_timeout, Duration::from_secs(300));
        assert_eq!(c.update_interval, Duration::from_secs(1));
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.log_format, LogFormat::Json);
        assert!(c.normalize);
        assert_eq!(c.normalize_lufs, -14);
        assert!(c.plugins.is_empty());
        assert!(c.state_file.is_none());
        assert!(!c.allow_no_voice);
        assert_eq!(
            c.sources.search_order,
            ["deezer", "applemusic", "spotify", "soundcloud"]
        );
        assert_eq!(c.sources.playback_order, ["deezer", "soundcloud"]);
        assert!(c.sources.http.enabled && c.sources.http.block_private);
        assert!(c.sources.audio_cache_dir.is_none());
        assert_eq!(c.sources.audio_cache_gb, 20);
    }

    #[test]
    fn overrides_and_clamps() {
        let c = load(&[
            ("WAVYZON_PASSWORD", "s3cret"),
            ("WAVYZON_BUFFER_MS", "100"),
            ("WAVYZON_RESUME_TIMEOUT_SECONDS", "60"),
            ("WAVYZON_PLAYOUT_DELAY_MS", "80"),
            ("WAVYZON_PAUSE_FADE_MS", "99999"),
            ("WAVYZON_UPDATE_INTERVAL_MS", "10"),
            ("WAVYZON_BITRATE", "1000000"),
            ("WAVYZON_SEARCH_ORDER", "deezer, spotify"),
            ("WAVYZON_NORMALIZE", "false"),
            ("WAVYZON_NORMALIZE_LUFS", "0"),
            ("WAVYZON_PLUGINS", " Example, ,other"),
            ("WAVYZON_LOG_FORMAT", "pretty"),
            ("WAVYZON_ALLOW_NO_VOICE", "yes"),
        ])
        .unwrap();
        assert_eq!(c.buffer_ms, 1_000);
        assert_eq!(c.resume_timeout, Duration::from_secs(60));
        assert_eq!(c.playout_delay, Duration::from_millis(80));
        assert_eq!(c.pause_fade, Duration::from_secs(2));
        assert_eq!(c.update_interval, Duration::from_millis(250));
        assert_eq!(c.bitrate, 384_000);
        assert_eq!(c.sources.search_order, ["deezer", "spotify"]);
        assert!(!c.normalize);
        assert_eq!(c.normalize_lufs, -5);
        assert_eq!(c.plugins, ["example", "other"]);
        assert_eq!(c.log_format, LogFormat::Pretty);
        assert!(c.allow_no_voice);
    }

    #[test]
    fn non_numbers_are_refused_with_the_variable_name() {
        let err = load(&[("WAVYZON_PASSWORD", "x"), ("WAVYZON_BITRATE", "fast")]).unwrap_err();
        assert_eq!(err.to_string(), "WAVYZON_BITRATE: \"fast\" is not a number");
        assert!(load(&[("WAVYZON_PASSWORD", "x"), ("WAVYZON_AUDIO_CACHE_GB", "lots")]).is_err());
        assert!(load(&[("WAVYZON_PASSWORD", "x"), ("WAVYZON_LOG_FORMAT", "xml")]).is_err());
    }

    #[test]
    fn documented_source_names_are_read() {
        let c = load(&[
            ("WAVYZON_PASSWORD", "x"),
            ("WAVYZON_PLAYBACK_SOURCES", "soundcloud, tone"),
            ("WAVYZON_HTTP_SOURCE", "false"),
            ("WAVYZON_HTTP_ALLOW_PRIVATE", "true"),
            ("WAVYZON_LOCAL_ROOT", "/srv/music"),
            ("WAVYZON_AUDIO_CACHE_DIR", "/var/cache/wavyzon"),
            ("WAVYZON_AUDIO_CACHE_GB", "0"),
        ])
        .unwrap();
        assert_eq!(c.sources.playback_order, ["soundcloud", "tone"]);
        assert!(!c.sources.http.enabled);
        assert!(!c.sources.http.block_private);
        assert_eq!(c.sources.local.root, Some(PathBuf::from("/srv/music")));
        assert_eq!(c.sources.audio_cache_dir, Some(PathBuf::from("/var/cache/wavyzon")));
        assert_eq!(c.sources.audio_cache_gb, 1);
    }

    #[test]
    fn peer_cache_needs_secret_and_disk_cache() {
        let c = load(&[
            ("WAVYZON_PASSWORD", "x"),
            ("WAVYZON_CACHE_PEERS", "https://a:2333, https://b:2333"),
        ])
        .unwrap();
        assert_eq!(c.cache_peers.len(), 2);
        assert!(!c.peer_cache_enabled());
        assert!(c.warnings().iter().any(|w| w.starts_with("WAVYZON_CACHE_PEERS")));
        let c = load(&[
            ("WAVYZON_PASSWORD", "x"),
            ("WAVYZON_CACHE_PEERS", "https://a:2333"),
            ("WAVYZON_CACHE_SECRET", "peer"),
            ("WAVYZON_AUDIO_CACHE_DIR", "/tmp/c"),
        ])
        .unwrap();
        assert!(c.peer_cache_enabled());
        assert!(!c.warnings().iter().any(|w| w.starts_with("WAVYZON_CACHE_PEERS")));
    }

    #[test]
    fn private_http_on_public_address_warns() {
        let c = load(&[("WAVYZON_PASSWORD", "x"), ("WAVYZON_HTTP_ALLOW_PRIVATE", "true")]).unwrap();
        assert!(c.warnings().iter().any(|w| w.starts_with("WAVYZON_HTTP_ALLOW_PRIVATE")));
        let c = load(&[
            ("WAVYZON_PASSWORD", "x"),
            ("WAVYZON_HTTP_ALLOW_PRIVATE", "true"),
            ("WAVYZON_ADDRESS", "127.0.0.1:2333"),
        ])
        .unwrap();
        assert!(!c.warnings().iter().any(|w| w.starts_with("WAVYZON_HTTP_ALLOW_PRIVATE")));
    }
}
