//! # wavyzon-plugin
//!
//! The plugin API of [Wavyzon](https://gitlab.com/ihrz/wavyzon), and the data
//! model every Wavyzon crate shares (tracks, failures, events, streams).
//!
//! A plugin implements [`Plugin`] and, in [`Plugin::register`], adds any of:
//!
//! | Extension | Trait | Exposed as |
//! | --- | --- | --- |
//! | Sources | [`Source`] (+ [`Playable`], [`IsrcLookup`], [`Recommender`]) | `loadtracks` prefixes and links, playback |
//! | Audio filters | [`FilterFactory`] → [`AudioFilter`] | `filters.pluginFilters.<name>` |
//! | Event listeners | [`EventListener`] | every player event |
//! | HTTP routes | [`axum::Router`] | `/v1/plugins/<name>/…` |
//! | Lyrics | [`LyricsProvider`] | `/v1/lyrics`, `/v4/lyrics` (asked before lyrics.ovh) |
//!
//! See `docs/src/plugins.md` in the repository for a full guide.

mod event;
mod failure;
mod filter;
mod lyrics;
mod plugin;
mod source;
mod track;

pub use async_trait::async_trait;
pub use axum;
pub use serde_json;

pub use event::{EndReason, Event, EventType};
pub use failure::{Failure, Severity};
pub use filter::{AudioFilter, CHANNELS, FRAME_SAMPLES, FilterFactory, SAMPLE_RATE};
pub use lyrics::{Lyrics, LyricsProvider};
pub use plugin::{API_VERSION, EventListener, Plugin, PluginContext, PluginError, PluginManifest, Registry};
pub use source::{Format, IsrcLookup, Playable, Recommender, Source, Stream};
pub use track::{LoadResult, LoadType, Playlist, Track};
