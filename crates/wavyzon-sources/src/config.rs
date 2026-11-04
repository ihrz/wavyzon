//! [`SourcesConfig`]: everything needed to build the built-in sources.
//!
//! Field docs name the environment variable each one mirrors; the server
//! fills the struct (or calls [`SourcesConfig::from_lookup`]).

use std::path::PathBuf;
use std::time::Duration;

use crate::http_source::HttpSourceConfig;

fn list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Deezer public catalog.
#[derive(Debug, Clone)]
pub struct DeezerConfig {
    /// `DEEZER_ENABLED` (default `true`).
    pub enabled: bool,
    /// `DEEZER_PROXY`.
    pub proxy: Option<String>,
    /// API base (tests); default `https://api.deezer.com/2.0`.
    pub api_base: String,
}

impl Default for DeezerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            proxy: None,
            api_base: "https://api.deezer.com/2.0".into(),
        }
    }
}

/// Spotify Web API (metadata only).
#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    /// `SPOTIFY_ENABLED` (default `true`). Without credentials the source
    /// stays registered but answers with a `fault` failure.
    pub enabled: bool,
    /// `SPOTIFY_CLIENT_ID`.
    pub client_id: String,
    /// `SPOTIFY_CLIENT_SECRET`.
    pub client_secret: String,
    /// `SPOTIFY_PROXY`.
    pub proxy: Option<String>,
    pub endpoints: crate::spotify::Endpoints,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            client_id: String::new(),
            client_secret: String::new(),
            proxy: None,
            endpoints: Default::default(),
        }
    }
}

/// Apple Music catalog (metadata only).
#[derive(Debug, Clone)]
pub struct AppleMusicConfig {
    /// `APPLE_MUSIC_ENABLED` (default `true`).
    pub enabled: bool,
    /// `APPLE_MUSIC_TOKEN`: fixed developer token; scraped when unset.
    pub token: Option<String>,
    /// `APPLE_MUSIC_STOREFRONT` (default `us`).
    pub storefront: String,
    /// `APPLE_MUSIC_PROXY`.
    pub proxy: Option<String>,
    pub endpoints: crate::applemusic::Endpoints,
}

impl Default for AppleMusicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token: None,
            storefront: "us".into(),
            proxy: None,
            endpoints: Default::default(),
        }
    }
}

/// SoundCloud (playable).
#[derive(Debug, Clone)]
pub struct SoundCloudConfig {
    /// `SOUNDCLOUD_ENABLED` (default `true`).
    pub enabled: bool,
    /// `SOUNDCLOUD_CLIENT_ID`: fixed client id; scraped when unset.
    pub client_id: Option<String>,
    /// `SOUNDCLOUD_PROXY`.
    pub proxy: Option<String>,
    pub endpoints: crate::soundcloud::Endpoints,
}

impl Default for SoundCloudConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            client_id: None,
            proxy: None,
            endpoints: Default::default(),
        }
    }
}

/// Local files.
#[derive(Debug, Clone, Default)]
pub struct LocalConfig {
    /// `WAVYZON_LOCAL_ROOT` (alias `LOCAL_SOURCE_ROOT`): the only directory served. Unset disables the
    /// source.
    pub root: Option<PathBuf>,
}

/// Configuration of every built-in source and of the resolver.
#[derive(Debug, Clone)]
pub struct SourcesConfig {
    /// `WAVYZON_SEARCH_ORDER` (default `applemusic,spotify,deezer,soundcloud`):
    /// sources queried, in order, for plain-text searches (first non-empty
    /// answer wins).
    pub search_order: Vec<String>,
    /// `WAVYZON_PLAYBACK_SOURCES` (alias `WAVYZON_PLAYBACK_ORDER`; default `soundcloud`): playable sources that
    /// metadata-only tracks are mirrored to, in order of preference. Playable
    /// plugin sources not listed are appended after these.
    pub playback_order: Vec<String>,
    /// `WAVYZON_AUDIO_CACHE_DIR`: disk cache of repeat-played tracks. Unset
    /// disables it.
    pub audio_cache_dir: Option<PathBuf>,
    /// `WAVYZON_AUDIO_CACHE_GB` (default 20, 1–1000).
    pub audio_cache_gb: u64,
    /// `WAVYZON_PREFETCH_ON_LOAD` (default `true`): preload the first result
    /// of every `load`.
    pub prefetch_on_load: bool,
    pub deezer: DeezerConfig,
    pub spotify: SpotifyConfig,
    pub apple_music: AppleMusicConfig,
    pub soundcloud: SoundCloudConfig,
    /// `WAVYZON_HTTP_SOURCE`, `WAVYZON_HTTP_ALLOW_PRIVATE` (inverse of
    /// `block_private`), `WAVYZON_HTTP_MAX_REDIRECTS`, `WAVYZON_HTTP_TIMEOUT_MS`,
    /// `WAVYZON_HTTP_PROXY` (aliases `HTTP_SOURCE_*`).
    pub http: HttpSourceConfig,
    pub local: LocalConfig,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            search_order: list("applemusic,spotify,deezer,soundcloud"),
            playback_order: list("soundcloud"),
            audio_cache_dir: None,
            audio_cache_gb: 20,
            prefetch_on_load: true,
            deezer: DeezerConfig::default(),
            spotify: SpotifyConfig::default(),
            apple_music: AppleMusicConfig::default(),
            soundcloud: SoundCloudConfig::default(),
            http: HttpSourceConfig::default(),
            local: LocalConfig::default(),
        }
    }
}

impl SourcesConfig {
    /// Read the process environment (see the field docs for names).
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from any `name → value` lookup; unset or empty values keep the
    /// default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let s = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        // `1/true/yes/on` and `0/false/no/off`; anything else keeps the
        // default, like Cadence (so a typo never silently disables a source).
        let flag = |k: &str, d: bool| match s(k).map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("1" | "true" | "yes" | "on") => true,
            Some("0" | "false" | "no" | "off") => false,
            _ => d,
        };
        let num = |k: &str, d: u64| s(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let mut c = Self::default();
        if let Some(v) = s("WAVYZON_SEARCH_ORDER") {
            c.search_order = list(&v);
        }
        // `WAVYZON_PLAYBACK_SOURCES` is the documented name; the others are
        // accepted as aliases (the first one set wins).
        let alias = |names: &[&str]| names.iter().find_map(|k| s(k));
        if let Some(v) = alias(&["WAVYZON_PLAYBACK_SOURCES", "WAVYZON_PLAYBACK_ORDER"]) {
            c.playback_order = list(&v);
        }
        c.audio_cache_dir = s("WAVYZON_AUDIO_CACHE_DIR").map(PathBuf::from);
        c.audio_cache_gb = num("WAVYZON_AUDIO_CACHE_GB", 20).clamp(1, 1000);
        c.prefetch_on_load = flag("WAVYZON_PREFETCH_ON_LOAD", true);
        c.deezer.enabled = flag("DEEZER_ENABLED", true);
        c.deezer.proxy = s("DEEZER_PROXY");
        c.spotify.enabled = flag("SPOTIFY_ENABLED", true);
        c.spotify.client_id = s("SPOTIFY_CLIENT_ID").unwrap_or_default();
        c.spotify.client_secret = s("SPOTIFY_CLIENT_SECRET").unwrap_or_default();
        c.spotify.proxy = s("SPOTIFY_PROXY");
        c.apple_music.enabled = flag("APPLE_MUSIC_ENABLED", true);
        c.apple_music.token = s("APPLE_MUSIC_TOKEN");
        c.apple_music.storefront = s("APPLE_MUSIC_STOREFRONT").unwrap_or_else(|| "us".into());
        c.apple_music.proxy = s("APPLE_MUSIC_PROXY");
        c.soundcloud.enabled = flag("SOUNDCLOUD_ENABLED", true);
        c.soundcloud.client_id = s("SOUNDCLOUD_CLIENT_ID");
        c.soundcloud.proxy = s("SOUNDCLOUD_PROXY");
        let enabled = if s("WAVYZON_HTTP_SOURCE").is_some() {
            "WAVYZON_HTTP_SOURCE"
        } else {
            "HTTP_SOURCE_ENABLED"
        };
        c.http.enabled = flag(enabled, true);
        c.http.block_private = match s("WAVYZON_HTTP_ALLOW_PRIVATE") {
            Some(_) => !flag("WAVYZON_HTTP_ALLOW_PRIVATE", false),
            None => flag("HTTP_SOURCE_BLOCK_PRIVATE", true),
        };
        let redirects = alias(&["WAVYZON_HTTP_MAX_REDIRECTS", "HTTP_SOURCE_MAX_REDIRECTS"]);
        c.http.max_redirects = redirects.and_then(|v| v.parse().ok()).unwrap_or(5);
        let timeout = alias(&["WAVYZON_HTTP_TIMEOUT_MS", "HTTP_SOURCE_TIMEOUT_MS"]).and_then(|v| v.parse().ok());
        c.http.timeout = Duration::from_millis(timeout.unwrap_or(10_000u64).max(500));
        c.http.proxy = alias(&["WAVYZON_HTTP_PROXY", "HTTP_SOURCE_PROXY"]);
        c.local.root = alias(&["WAVYZON_LOCAL_ROOT", "LOCAL_SOURCE_ROOT"]).map(PathBuf::from);
        c
    }

    /// Human-readable warnings about the configuration (missing credentials…).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.spotify.enabled && (self.spotify.client_id.is_empty() || self.spotify.client_secret.is_empty()) {
            out.push("SPOTIFY_CLIENT_ID/SECRET are not set: Spotify links will not resolve".into());
        }
        if !self.soundcloud.enabled && self.playback_order.iter().all(|p| p == "soundcloud") {
            out.push("SoundCloud is disabled and no other playback source is configured: only http/local tracks and plugin sources can play".into());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn unrecognized_flags_keep_the_default() {
        let env: HashMap<&str, &str> = [("SOUNDCLOUD_ENABLED", "enable"), ("DEEZER_ENABLED", "Off")].into();
        let c = SourcesConfig::from_lookup(|k| env.get(k).map(|v| v.to_string()));
        assert!(c.soundcloud.enabled);
        assert!(!c.deezer.enabled);
    }

    #[test]
    fn defaults_and_env() {
        let d = SourcesConfig::default();
        assert_eq!(d.search_order, ["applemusic", "spotify", "deezer", "soundcloud"]);
        assert_eq!(d.playback_order, ["soundcloud"]);
        assert!(d.http.block_private);
        let env: HashMap<&str, &str> = [
            ("WAVYZON_SEARCH_ORDER", "Deezer, soundcloud"),
            ("WAVYZON_AUDIO_CACHE_GB", "5000"),
            ("HTTP_SOURCE_BLOCK_PRIVATE", "false"),
            ("SOUNDCLOUD_ENABLED", "0"),
            ("WAVYZON_LOCAL_ROOT", "/music"),
            ("SPOTIFY_PROXY", " "),
        ]
        .into();
        let c = SourcesConfig::from_lookup(|k| env.get(k).map(|v| v.to_string()));
        assert_eq!(c.search_order, ["deezer", "soundcloud"]);
        assert_eq!(c.audio_cache_gb, 1000);
        assert!(!c.http.block_private);
        assert!(!c.soundcloud.enabled);
        assert_eq!(c.local.root, Some(PathBuf::from("/music")));
        assert_eq!(c.spotify.proxy, None);
        assert!(!c.warnings().is_empty());
        let documented: HashMap<&str, &str> = [
            ("WAVYZON_HTTP_ALLOW_PRIVATE", "true"),
            ("WAVYZON_HTTP_SOURCE", "off"),
            ("WAVYZON_PLAYBACK_SOURCES", "tone"),
        ]
        .into();
        let c = SourcesConfig::from_lookup(|k| documented.get(k).map(|v| v.to_string()));
        assert!(!c.http.block_private);
        assert!(!c.http.enabled);
        assert_eq!(c.playback_order, ["tone"]);
    }
}
