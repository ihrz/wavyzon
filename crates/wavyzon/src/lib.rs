//! # wavyzon
//!
//! The Wavyzon node: configuration, client sessions, the `/v1` API, the
//! Lavalink v4 layer, metrics, the peer cache and plugin loading. The
//! `wavyzon` binary is a thin `main` around [`node::Node`].
//!
//! Ported from Cadence's `cmd/cadence` and `internal/{api, session, lavalink,
//! config, metrics, peercache}`.

pub mod api;
pub mod config;
pub mod lavalink;
pub mod metrics;
pub mod node;
pub mod peercache;
pub mod plugins;
pub mod session;

pub use config::Config;
pub use node::Node;
