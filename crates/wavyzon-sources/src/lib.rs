//! # wavyzon-sources
//!
//! The built-in sources of [Wavyzon](https://gitlab.com/ihrz/wavyzon) and the
//! resolver that ties them (and plugin sources) together.
//!
//! | Source | Prefix | Links | Audio |
//! | --- | --- | --- | --- |
//! | [`soundcloud`] | `scsearch:` | tracks, sets, short links | ✅ (mirror target) |
//! | [`deezer`] | `dzsearch:` / `dzisrc:` / `dzrec:` | tracks, albums, playlists, artists, short links | ✅ with `DEEZER_ARL` + `DEEZER_MASTER_KEY`, else mirrored |
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
//! Ported from Cadence (Go), including Deezer streaming: the `DEEZER_ARL`
//! login, media URLs, `BF_CBC_STRIPE` decryption and range seeks. Wavyzon
//! generalizes Cadence's per-Deezer head/disk caches to every source in the
//! resolver, and keeps Deezer metadata-only (mirroring the tracks instead)
//! when no ARL or master key is configured.

pub mod applemusic;
pub mod audiocache;
mod builtins;
pub mod cache;
pub mod config;
pub mod deezer;
pub mod http_source;
pub mod httpx;
pub mod local;
pub mod lyrics;
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
/// Streaming-response helpers, shared with plugin sources that stream over
/// [`httpx::HttpClient`] (`wavyzon-plugin-flowery`).
pub use util::{Body, response_body};
