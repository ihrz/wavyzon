//! Construction of the built-in sources from a [`SourcesConfig`].

use std::sync::Arc;

use wavyzon_plugin::Source;

use crate::applemusic::AppleMusic;
use crate::audiocache::{CacheError, Store};
use crate::config::SourcesConfig;
use crate::deezer::Deezer;
use crate::http_source::HttpSource;
use crate::httpx::{HttpClient, HttpError, HttpOptions};
use crate::local::LocalSource;
use crate::resolver::{Sources, SourcesBuilder};
use crate::soundcloud::SoundCloud;
use crate::spotify::Spotify;

/// Why [`Sources::builder`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("{source_name} http client: {error}")]
    Http {
        source_name: &'static str,
        error: HttpError,
    },
    #[error("audio cache: {0}")]
    AudioCache(#[from] CacheError),
    #[error("local source root: {0}")]
    LocalRoot(std::io::Error),
}

fn client(name: &'static str, proxy: &Option<String>) -> Result<Arc<HttpClient>, BuildError> {
    HttpClient::new(name, HttpOptions::with_proxy(proxy.clone()))
        .map(Arc::new)
        .map_err(|error| BuildError::Http {
            source_name: name,
            error,
        })
}

impl Sources {
    /// A builder with every enabled built-in source constructed from `cfg`
    /// (and the disk audio cache opened). Add plugin sources with
    /// [`SourcesBuilder::plugin`], then call [`SourcesBuilder::build`].
    pub fn builder(cfg: &SourcesConfig) -> Result<SourcesBuilder, BuildError> {
        let mut b = SourcesBuilder::bare(cfg.search_order.clone(), cfg.playback_order.clone())
            .prefetch_on_load(cfg.prefetch_on_load);
        if cfg.soundcloud.enabled {
            let sc = SoundCloud::new(
                client(crate::soundcloud::NAME, &cfg.soundcloud.proxy)?,
                cfg.soundcloud.client_id.clone(),
                cfg.soundcloud.endpoints.clone(),
            );
            b = b.builtin(Arc::new(sc));
        }
        if cfg.deezer.enabled {
            b = b.builtin(Arc::new(Deezer::new(
                client(crate::deezer::NAME, &cfg.deezer.proxy)?,
                &cfg.deezer.api_base,
            )));
        }
        if cfg.spotify.enabled {
            let sp = Spotify::new(
                client(crate::spotify::NAME, &cfg.spotify.proxy)?,
                &cfg.spotify.client_id,
                &cfg.spotify.client_secret,
                cfg.spotify.endpoints.clone(),
            );
            b = b.builtin(Arc::new(sp));
        }
        if cfg.apple_music.enabled {
            let am = AppleMusic::new(
                client(crate::applemusic::NAME, &cfg.apple_music.proxy)?,
                cfg.apple_music.token.clone(),
                &cfg.apple_music.storefront,
                cfg.apple_music.endpoints.clone(),
            );
            b = b.builtin(Arc::new(am));
        }
        if let Some(root) = &cfg.local.root {
            let local = LocalSource::new(root).map_err(BuildError::LocalRoot)?;
            tracing::info!(root = %local.root().display(), "local source enabled");
            b = b.builtin(Arc::new(local));
        }
        if cfg.http.enabled {
            let http = HttpSource::new(&cfg.http).map_err(|error| BuildError::Http {
                source_name: crate::http_source::NAME,
                error,
            })?;
            b = b.catch_all(Arc::new(http) as Arc<dyn Source>);
        }
        if let Some(dir) = &cfg.audio_cache_dir {
            let store = Store::open(dir, cfg.audio_cache_gb.clamp(1, 1000) << 30, b.metrics_handle())?;
            b = b.audio_cache(Arc::new(store));
        }
        for w in cfg.warnings() {
            tracing::warn!("{w}");
        }
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[tokio::test]
    async fn builds_every_builtin() {
        let d = TempDir::new();
        let cfg = SourcesConfig {
            audio_cache_dir: Some(d.path().join("cache")),
            local: crate::config::LocalConfig {
                root: Some(d.path().to_path_buf()),
            },
            ..Default::default()
        };
        let s = Sources::builder(&cfg).unwrap().build();
        assert_eq!(
            s.names(),
            ["soundcloud", "deezer", "spotify", "applemusic", "local", "http"]
        );
        assert_eq!(s.playback_names(), ["soundcloud"]);
        assert!(s.audio_cache().is_some());
        let bad = SourcesConfig {
            local: crate::config::LocalConfig {
                root: Some(d.path().join("missing")),
            },
            ..Default::default()
        };
        assert!(matches!(Sources::builder(&bad), Err(BuildError::LocalRoot(_))));
    }
}
