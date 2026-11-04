//! # wavyzon-sources
//!
//! The built-in sources of [Wavyzon](https://gitlab.com/ihrz/wavyzon) and the
//! resolver that ties them (and plugin sources) together.
//!
//! | Source | Prefix | Links | Audio |
//! | --- | --- | --- | --- |
//! | [`soundcloud`] | `scsearch:` | tracks, sets, short links | ✅ (default mirror target) |
//! | [`deezer`] | `dzsearch:` / `dzisrc:` / `dzrec:` | tracks, albums, playlists, artists, short links | metadata, mirrored |
//! | [`spotify`] | `spsearch:` | tracks, albums, playlists, `spotify:` URIs, short links | metadata, mirrored |
//! | [`applemusic`] | `amsearch:` | songs, albums, playlists, artists | metadata, mirrored |
//! | [`http_source`] | — | any other `http(s)://` file or Icecast/Shoutcast stream | ✅ |
//! | [`local`] | `local:` | files/directories under a configured root | ✅ |
//!
//! Build everything with [`Sources::builder`] from a [`SourcesConfig`], add
//! plugin sources with [`SourcesBuilder::plugin`], then use [`Sources::load`],
//! [`Sources::playable`], [`Sources::open`], [`Sources::prefetch`] and
//! [`Sources::recommend`]. Counters live in [`Metrics`].
//!
//! Ported from Cadence (Go). Unlike Cadence, Deezer is metadata-only here:
//! account login, media URLs and stream decryption are intentionally not
//! implemented.

pub mod applemusic;
pub mod audiocache;
mod builtins;
pub mod cache;
pub mod config;
pub mod deezer;
pub mod http_source;
pub mod httpx;
pub mod local;
pub mod matching;
pub mod metrics;
pub mod probe;
pub mod resolver;
pub mod soundcloud;
pub mod spotify;
pub mod streams;
mod util;

#[cfg(test)]
mod testutil;

pub use builtins::BuildError;
pub use config::{AppleMusicConfig, DeezerConfig, LocalConfig, SoundCloudConfig, SourcesConfig, SpotifyConfig};
pub use http_source::HttpSourceConfig;
pub use metrics::{Metrics, Sample};
pub use resolver::{Sources, SourcesBuilder};
