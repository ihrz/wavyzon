//! [`Sources`]: the registry of every source (plugins + built-ins) and the
//! resolver API the server uses.
//!
//! # Loading (`load`)
//!
//! 1. **Links**: the first source whose `matches_url` accepts the identifier
//!    loads it. Plugin sources are asked before the built-ins. Any other
//!    `http(s)://` URL goes to the `http` source when it is enabled.
//! 2. **ISRC**: `<x>isrc:<ISRC>` (e.g. `dzisrc:`) is looked up on the
//!    ISRC-capable sources, playback sources first.
//! 3. **Recommendations**: `<x>rec:<seed>` asks the recommender whose search
//!    prefix is `<x>search` (e.g. `dzrec:` → Deezer); fan edits are dropped.
//! 4. **Search prefixes** (`scsearch:`, `dzsearch:`, plugin prefixes…).
//! 5. `ytsearch:`/`ytmsearch:` (when no plugin claims them) and plain text:
//!    the configured search order, first non-empty answer wins.
//!
//! An empty search on a playback source falls back to the other playback
//! sources. Results are re-ranked so versions the query did not ask for
//! (live, karaoke…) come last. Non-error results are cached for 10 minutes.
//!
//! # Playing (`playable`, `open`)
//!
//! Metadata-only tracks (Spotify, Apple Music, Deezer, any non-playable
//! plugin source) are **mirrored**: first by ISRC on the ISRC-capable playback
//! sources (±5 s), then by a scored text search on each playback source in
//! order. Matches are cached 14 days, misses 30 minutes, and concurrent
//! lookups of the same track share one resolution.
//!
//! `open` then serves, in order: the disk audio cache, a prefetched head
//! (first ~12 s, served from memory while the rest is fetched), or the
//! source itself. A track played a second time within 7 days is written to
//! the disk cache as it plays.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use wavyzon_plugin::{Failure, Format, LoadResult, LoadType, Lyrics, LyricsProvider, Source, Stream, Track};

use crate::audiocache::Store;
use crate::cache::Lru;
use crate::lyrics::LyricsClient;
use crate::matching::{best_match, fan_edit, normalize_isrc, rank_for_query, within_drift};
use crate::metrics::{Metrics, inc};
use crate::streams::{HeadReader, TeeReader};
use crate::util::Body;

const MIRROR_CACHE_SIZE: usize = 200_000;
const MIRROR_CACHE_TTL: Duration = Duration::from_secs(14 * 24 * 3600);
const MISS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const LOAD_CACHE_SIZE: usize = 20_000;
const LOAD_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Max tracks waiting to be prefetched.
pub const PREFETCH_PENDING: usize = 64;
/// Concurrent prefetches.
pub const PREFETCH_WORKERS: usize = 4;
const PREFETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes kept per prefetched track: ~12 s at 128 kbit/s.
pub const HEAD_BYTES: usize = 192 * 1024;
const HEAD_ENTRIES: usize = 256;
const HEAD_TTL: Duration = Duration::from_secs(10 * 60);
const PLAYS_TO_CACHE: u32 = 2;
const PLAY_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);
const PLAY_ENTRIES: usize = 200_000;
/// Recommendations asked for by the `<x>rec:` prefix.
pub const RECOMMEND_LIMIT: usize = 50;
/// Search prefixes treated as plain-text searches when no source claims them.
pub const SEARCH_ALIASES: &[&str] = &["ytmsearch:", "ytsearch:"];

type Flight = Shared<BoxFuture<'static, Result<Track, Failure>>>;

struct Head {
    data: Bytes,
    complete: bool,
    format: Format,
}

struct DiskCache {
    store: Arc<Store>,
    plays: Lru<String, u32>,
}

struct Inner {
    ordered: Vec<Arc<dyn Source>>,
    by_name: HashMap<String, Arc<dyn Source>>,
    catch_all: Option<Arc<dyn Source>>,
    search_order: Vec<String>,
    playback: Vec<Arc<dyn Source>>,
    prefetch_on_load: bool,
    mirrors: Lru<String, Option<Track>>,
    loads: Lru<String, LoadResult>,
    inflight: Mutex<HashMap<String, Flight>>,
    warming: Mutex<HashSet<String>>,
    warm_slots: Arc<Semaphore>,
    heads: Lru<String, Arc<Head>>,
    disk: Option<DiskCache>,
    /// Plugin providers first, lyrics.ovh last.
    lyrics: Vec<Arc<dyn LyricsProvider>>,
    /// How many of `lyrics` come from plugins (counted in `lyrics_plugin_total`).
    lyrics_plugins: usize,
    metrics: Arc<Metrics>,
}

/// Builds a [`Sources`].
pub struct SourcesBuilder {
    builtins: Vec<Arc<dyn Source>>,
    plugins: Vec<Arc<dyn Source>>,
    catch_all: Option<Arc<dyn Source>>,
    search_order: Vec<String>,
    playback_order: Vec<String>,
    prefetch_on_load: bool,
    audio_cache: Option<Arc<Store>>,
    lyrics: Option<Arc<LyricsClient>>,
    lyrics_plugins: Vec<Arc<dyn LyricsProvider>>,
    metrics: Arc<Metrics>,
}

impl SourcesBuilder {
    /// A builder with no sources (tests, embedders).
    pub fn bare(search_order: Vec<String>, playback_order: Vec<String>) -> Self {
        Self {
            builtins: vec![],
            plugins: vec![],
            catch_all: None,
            search_order,
            playback_order,
            prefetch_on_load: false,
            audio_cache: None,
            lyrics: None,
            lyrics_plugins: vec![],
            metrics: Arc::new(Metrics::default()),
        }
    }

    /// Add a built-in source (consulted after plugins).
    pub fn builtin(mut self, s: Arc<dyn Source>) -> Self {
        self.builtins.push(s);
        self
    }

    /// Add a plugin source: consulted first for links, its prefixes usable,
    /// and, if playable, a mirror target after the configured playback order.
    /// A plugin source with the name of a built-in replaces it.
    pub fn plugin(mut self, s: Arc<dyn Source>) -> Self {
        self.plugins.push(s);
        self
    }

    pub fn plugins(mut self, it: impl IntoIterator<Item = Arc<dyn Source>>) -> Self {
        self.plugins.extend(it);
        self
    }

    /// The source asked for `http(s)://` links nobody else matched.
    pub fn catch_all(mut self, s: Arc<dyn Source>) -> Self {
        self.catch_all = Some(s);
        self
    }

    pub fn audio_cache(mut self, store: Arc<Store>) -> Self {
        self.audio_cache = Some(store);
        self
    }

    pub fn prefetch_on_load(mut self, on: bool) -> Self {
        self.prefetch_on_load = on;
        self
    }

    /// Share an existing metrics registry (the audio cache store should use
    /// the same one).
    pub fn metrics(mut self, m: Arc<Metrics>) -> Self {
        self.metrics = m;
        self
    }

    /// Serve lyrics through this lyrics.ovh client, after any plugin
    /// provider (no client and no provider: lyrics are off).
    pub fn lyrics(mut self, client: Arc<LyricsClient>) -> Self {
        self.lyrics = Some(client);
        self
    }

    /// Lyrics providers registered by plugins, asked in order before lyrics.ovh.
    pub fn lyrics_providers(mut self, it: impl IntoIterator<Item = Arc<dyn LyricsProvider>>) -> Self {
        self.lyrics_plugins.extend(it);
        self
    }

    pub fn metrics_handle(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    pub fn build(self) -> Sources {
        let plugin_names: HashSet<String> = self.plugins.iter().map(|p| p.name().to_string()).collect();
        let mut ordered: Vec<Arc<dyn Source>> = self.plugins.clone();
        ordered.extend(
            self.builtins
                .iter()
                .filter(|b| !plugin_names.contains(b.name()))
                .cloned(),
        );
        let mut by_name = HashMap::new();
        for s in &ordered {
            by_name.entry(s.name().to_string()).or_insert_with(|| s.clone());
        }
        if let Some(c) = &self.catch_all {
            by_name.entry(c.name().to_string()).or_insert_with(|| c.clone());
        }
        let can_mirror =
            |s: &Arc<dyn Source>| s.playable().is_some() && (s.search_prefix().is_some() || s.isrc_lookup().is_some());
        let mut playback: Vec<Arc<dyn Source>> = Vec::new();
        for name in &self.playback_order {
            match by_name.get(name) {
                Some(s) if can_mirror(s) => playback.push(s.clone()),
                Some(_) => tracing::warn!(source = %name, "playback source cannot play or search, ignored"),
                None => tracing::debug!(source = %name, "playback source not registered"),
            }
        }
        for s in self.plugins.iter().chain(self.builtins.iter()) {
            if can_mirror(s)
                && !playback.iter().any(|p| p.name() == s.name())
                && by_name.get(s.name()).is_some_and(|b| Arc::ptr_eq(b, s))
            {
                playback.push(s.clone());
            }
        }
        let lyrics_plugins = self.lyrics_plugins.len();
        let mut lyrics = self.lyrics_plugins;
        if let Some(ovh) = self.lyrics {
            lyrics.push(ovh);
        }
        tracing::info!(
            lyrics = ?lyrics.iter().map(|p| p.name().to_string()).collect::<Vec<_>>(),
            sources = ?ordered.iter().map(|s| s.name()).collect::<Vec<_>>(),
            playback = ?playback.iter().map(|s| s.name()).collect::<Vec<_>>(),
            "sources ready"
        );
        Sources(Arc::new(Inner {
            ordered,
            by_name,
            catch_all: self.catch_all,
            search_order: self.search_order,
            playback,
            prefetch_on_load: self.prefetch_on_load,
            mirrors: Lru::new(MIRROR_CACHE_SIZE, MIRROR_CACHE_TTL),
            loads: Lru::new(LOAD_CACHE_SIZE, LOAD_CACHE_TTL),
            inflight: Mutex::new(HashMap::new()),
            warming: Mutex::new(HashSet::new()),
            warm_slots: Arc::new(Semaphore::new(PREFETCH_WORKERS)),
            heads: Lru::new(HEAD_ENTRIES, HEAD_TTL),
            disk: self.audio_cache.map(|store| DiskCache {
                store,
                plays: Lru::new(PLAY_ENTRIES, PLAY_WINDOW),
            }),
            lyrics,
            lyrics_plugins,
            metrics: self.metrics,
        }))
    }
}

/// The source registry and resolver. Cheap to clone.
#[derive(Clone)]
pub struct Sources(Arc<Inner>);

fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// `<x>isrc:<code>` → normalized 12-character ISRC.
fn isrc_identifier(id: &str) -> Option<String> {
    let (prefix, rest) = id.split_once(':')?;
    if !prefix.ends_with("isrc") {
        return None;
    }
    let isrc = normalize_isrc(rest);
    (isrc.len() == 12).then_some(isrc)
}

fn ext_for(format: &Format) -> String {
    match format {
        Format::Encoded { hint: Some(h) } => {
            let h = crate::probe::hint_for_mime(h).unwrap_or(h);
            let clean: String = h.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
            if clean.is_empty() { "bin".into() } else { clean }
        }
        Format::Pcm48 => "pcm".into(),
        Format::Encoded { hint: None } => "bin".into(),
    }
}

impl Sources {
    pub fn builder_bare(search_order: Vec<String>, playback_order: Vec<String>) -> SourcesBuilder {
        SourcesBuilder::bare(search_order, playback_order)
    }

    /// Names of every registered source (plugins first), plus the catch-all.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.ordered.iter().map(|s| s.name().to_string()).collect();
        if let Some(c) = &self.0.catch_all {
            v.push(c.name().to_string());
        }
        v
    }

    /// Names of the playback (mirror target) sources, in order.
    pub fn playback_names(&self) -> Vec<String> {
        self.0.playback.iter().map(|s| s.name().to_string()).collect()
    }

    pub fn source(&self, name: &str) -> Option<Arc<dyn Source>> {
        self.0.by_name.get(name).cloned()
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.0.metrics.clone()
    }

    /// Whether any lyrics provider is configured (plugins or lyrics.ovh).
    pub fn lyrics_enabled(&self) -> bool {
        !self.0.lyrics.is_empty()
    }

    /// Names of the lyrics providers, in the order they are asked.
    pub fn lyrics_providers(&self) -> Vec<String> {
        self.0.lyrics.iter().map(|p| p.name().to_string()).collect()
    }

    /// Lyrics of `track` from the first provider that has them: plugin
    /// providers (Genius) in order, then lyrics.ovh. A provider that fails
    /// is logged and skipped. `Ok(None)` when at least one provider answered
    /// and none had lyrics; the last error when every provider failed.
    pub async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure> {
        let mut failure = None;
        let mut answered = false;
        for (i, p) in self.0.lyrics.iter().enumerate() {
            let plugin = i < self.0.lyrics_plugins;
            match p.lyrics(track).await {
                Ok(Some(found)) => {
                    if plugin {
                        inc(&self.0.metrics.lyrics_plugins.found);
                    }
                    return Ok(Some(found));
                }
                Ok(None) => {
                    answered = true;
                    if plugin {
                        inc(&self.0.metrics.lyrics_plugins.not_found);
                        tracing::debug!(provider = p.name(), "no lyrics, asking the next provider");
                    }
                }
                Err(e) => {
                    if plugin {
                        inc(&self.0.metrics.lyrics_plugins.failed);
                        tracing::warn!(provider = p.name(), error = %e, "lyrics provider failed, asking the next one");
                    }
                    failure = Some(e);
                }
            }
        }
        match failure {
            Some(e) if !answered => Err(e),
            _ => Ok(None),
        }
    }

    /// The disk audio cache, if configured.
    pub fn audio_cache(&self) -> Option<Arc<Store>> {
        self.0.disk.as_ref().map(|d| d.store.clone())
    }

    /// Decode an encoded track.
    pub fn decode(&self, encoded: &str) -> Result<Track, Failure> {
        Track::decode(encoded)
    }

    /// Resolve a `loadtracks` identifier (see the module docs).
    pub async fn load(&self, identifier: &str) -> LoadResult {
        let id = identifier.trim();
        if id.is_empty() {
            return LoadResult::empty();
        }
        let key = id.to_string();
        let res = match self.0.loads.get(&key) {
            Some(cached) => cached,
            None => {
                let res = self.load_inner(id).await;
                if res.kind != LoadType::Error {
                    self.0.loads.set(key, res.clone());
                }
                res
            }
        };
        if self.0.prefetch_on_load {
            if let Some(first) = res.tracks.first() {
                self.enqueue(first.clone());
            }
        }
        res
    }

    async fn load_inner(&self, id: &str) -> LoadResult {
        for s in &self.0.ordered {
            if s.matches_url(id) {
                return s.load_url(id).await.unwrap_or_else(LoadResult::error);
            }
        }
        if is_url(id) {
            return match &self.0.catch_all {
                Some(c) if c.matches_url(id) => c.load_url(id).await.unwrap_or_else(LoadResult::error),
                _ => LoadResult::error(Failure::common("", "no source handles this link", None)),
            };
        }
        if let Some(isrc) = isrc_identifier(id) {
            return self.load_isrc(&isrc).await;
        }
        if let Some((prefix, seed)) = id.split_once(':') {
            if let Some(rec_prefix) = prefix.strip_suffix("rec") {
                if let Some(s) =
                    self.0.ordered.iter().find(|s| {
                        s.recommender().is_some() && s.search_prefix() == Some(&format!("{rec_prefix}search"))
                    })
                {
                    return self.recommend_load(s.clone(), seed.trim()).await;
                }
            }
        }
        for s in &self.0.ordered {
            if let Some(p) = s.search_prefix() {
                if let Some(q) = id.strip_prefix(p).and_then(|r| r.strip_prefix(':')) {
                    return self.search(s.clone(), q.trim()).await;
                }
            }
        }
        let mut query = id;
        for alias in SEARCH_ALIASES {
            if let Some(q) = id.strip_prefix(alias) {
                query = q.trim();
                break;
            }
        }
        let mut last = LoadResult::empty();
        for name in &self.0.search_order {
            let Some(s) = self.0.by_name.get(name) else { continue };
            let res = self.search(s.clone(), query).await;
            if res.kind == LoadType::Search {
                return res;
            }
            last = res;
        }
        last
    }

    async fn load_isrc(&self, isrc: &str) -> LoadResult {
        let mut candidates: Vec<&Arc<dyn Source>> = self.0.playback.iter().collect();
        candidates.extend(
            self.0
                .ordered
                .iter()
                .filter(|s| !self.0.playback.iter().any(|p| Arc::ptr_eq(p, s))),
        );
        let mut err = None;
        for s in candidates {
            let Some(l) = s.isrc_lookup() else { continue };
            match l.by_isrc(isrc).await {
                Ok(t) => return LoadResult::single(t),
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        }
        err.map(LoadResult::error).unwrap_or_else(LoadResult::empty)
    }

    async fn recommend_load(&self, s: Arc<dyn Source>, seed: &str) -> LoadResult {
        if seed.is_empty() {
            return LoadResult::empty();
        }
        let Some(r) = s.recommender() else {
            return LoadResult::empty();
        };
        match r.recommend(seed, RECOMMEND_LIMIT).await {
            Ok(found) => {
                let tracks: Vec<Track> = found.into_iter().filter(|t| !fan_edit(t)).collect();
                if tracks.is_empty() {
                    inc(&self.0.metrics.recommend.empty);
                } else {
                    inc(&self.0.metrics.recommend.served);
                }
                LoadResult::results(tracks)
            }
            Err(e) => {
                inc(&self.0.metrics.recommend.failed);
                LoadResult::error(e)
            }
        }
    }

    fn is_playback(&self, s: &Arc<dyn Source>) -> bool {
        self.0.playback.iter().any(|p| Arc::ptr_eq(p, s))
    }

    async fn search(&self, s: Arc<dyn Source>, query: &str) -> LoadResult {
        if query.is_empty() {
            return LoadResult::empty();
        }
        let res = s.search(query).await;
        let empty = res.as_ref().map(|t| t.is_empty()).unwrap_or(true);
        if empty && self.is_playback(&s) {
            // Only sources that look up free text: a `tone` source decodes a
            // frequency, a song title there is only an "invalid tone" warning.
            for fb in self
                .0
                .playback
                .iter()
                .filter(|p| !Arc::ptr_eq(p, &s) && p.searches_text())
            {
                match fb.search(query).await {
                    Ok(found) if !found.is_empty() => {
                        inc(&self.0.metrics.fallback.search);
                        tracing::info!(query, primary = s.name(), fallback = fb.name(), primary_error = ?res.as_ref().err(), "search answered by a fallback source");
                        return LoadResult::results(rank_for_query(query, found));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(query, fallback = fb.name(), error = %e, "fallback search failed"),
                }
            }
        }
        match res {
            Ok(tracks) => LoadResult::results(rank_for_query(query, tracks)),
            Err(e) => LoadResult::error(e),
        }
    }

    fn player_for(&self, t: &Track) -> Option<Arc<dyn Source>> {
        self.0
            .by_name
            .get(&t.source)
            .filter(|s| s.playable().is_some())
            .cloned()
    }

    /// The playable track for `t`: itself when its source plays audio,
    /// otherwise its mirror on a playback source.
    pub async fn playable(&self, t: &Track) -> Result<Track, Failure> {
        if self.player_for(t).is_some() {
            return Ok(t.clone());
        }
        if self.0.playback.is_empty() {
            return Err(Failure::fault(
                t.source.clone(),
                "no playback source is configured",
                None,
            ));
        }
        let key = t.key();
        if let Some(entry) = self.0.mirrors.get(&key) {
            inc(&self.0.metrics.mirror.cached);
            return entry.ok_or_else(|| self.no_match(t));
        }
        let flight = {
            let mut inflight = self.0.inflight.lock();
            inflight
                .entry(key.clone())
                .or_insert_with(|| {
                    let me = self.clone();
                    let t = t.clone();
                    async move { me.mirror(t).await }.boxed().shared()
                })
                .clone()
        };
        let res = flight.clone().await;
        let mut inflight = self.0.inflight.lock();
        if inflight.get(&key).is_some_and(|f| f.ptr_eq(&flight)) {
            inflight.remove(&key);
        }
        res
    }

    fn no_match(&self, t: &Track) -> Failure {
        let names: Vec<&str> = self.0.playback.iter().map(|s| s.name()).collect();
        Failure::common(
            t.source.clone(),
            format!("no matching recording on {}", names.join(", ")),
            None,
        )
    }

    async fn mirror(&self, mut t: Track) -> Result<Track, Failure> {
        let key = t.key();
        if t.isrc.is_empty() {
            if let Some(s) = self.0.by_name.get(&t.source) {
                if let Ok(full) = s.track(&t.identifier).await {
                    if !full.isrc.is_empty() {
                        t.isrc = full.isrc;
                        if t.duration_ms == 0 {
                            t.duration_ms = full.duration_ms;
                        }
                    }
                }
            }
        }
        t.isrc = normalize_isrc(&t.isrc);
        if !t.isrc.is_empty() {
            for (i, p) in self.0.playback.iter().enumerate() {
                let Some(l) = p.isrc_lookup() else { continue };
                if let Ok(hit) = l.by_isrc(&t.isrc).await {
                    if within_drift(t.duration_ms, hit.duration_ms) {
                        inc(&self.0.metrics.mirror.isrc);
                        if i > 0 {
                            inc(&self.0.metrics.fallback.mirror);
                        }
                        self.0.mirrors.set(key, Some(hit.clone()));
                        return Ok(hit);
                    }
                }
            }
        }
        let query = format!("{} {}", t.title, t.author).trim().to_string();
        let mut first_err: Option<Failure> = None;
        for (i, p) in self.0.playback.iter().enumerate() {
            match p.search(&query).await {
                Ok(candidates) => {
                    if let Some(hit) = best_match(&t, &candidates) {
                        inc(&self.0.metrics.mirror.search);
                        if i > 0 {
                            inc(&self.0.metrics.fallback.mirror);
                            tracing::info!(track = %key, fallback = p.name(), matched = %hit.key(), "mirrored to a fallback source");
                        }
                        let ttl = if first_err.is_some() {
                            MISS_CACHE_TTL
                        } else {
                            MIRROR_CACHE_TTL
                        };
                        self.0.mirrors.set_for(key, Some(hit.clone()), ttl);
                        return Ok(hit);
                    }
                }
                Err(e) => {
                    tracing::warn!(track = %key, source = p.name(), error = %e, "mirror search failed");
                    // Only the primary's error is the answer (as in
                    // Cadence): if it searched fine and found nothing, a
                    // failing fallback still makes this a cached miss.
                    if i == 0 {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            inc(&self.0.metrics.mirror.failed);
            return Err(e);
        }
        inc(&self.0.metrics.mirror.miss);
        self.0.mirrors.set_for(key, None, MISS_CACHE_TTL);
        Err(self.no_match(&t))
    }

    /// The disk cache key prefix of a playable track (`<40 hex>`; cache files
    /// are named `<prefix>-<ext>`). Peer caches look entries up by it.
    pub fn disk_key(t: &Track) -> String {
        let digest = Sha256::digest(t.key().as_bytes());
        hex::encode(digest)[..40].to_string()
    }

    /// Open `t` (mirroring it first when needed), ideally at `start_ms`.
    pub async fn open(&self, t: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let playable = self.playable(t).await?;
        let src = self
            .player_for(&playable)
            .ok_or_else(|| Failure::fault(playable.source.clone(), "no audio source plays this track", None))?;
        let player = src.playable().expect("player_for checks playable");
        let cacheable = !playable.is_stream && playable.source != crate::local::NAME;
        let start_ms = start_ms.max(0);

        if let (Some(disk), true) = (&self.0.disk, cacheable) {
            let hash = Self::disk_key(&playable);
            if let Some(key) = disk.store.find_prefix(&format!("{hash}-")) {
                if let Some((file, _)) = disk.store.get(&key) {
                    let ext = key.rsplit('-').next().unwrap_or("bin");
                    let hint = (ext != "bin").then(|| ext.to_string());
                    let format = if ext == "pcm" {
                        Format::Pcm48
                    } else {
                        Format::Encoded { hint }
                    };
                    tracing::debug!(track = %playable.key(), "playing from the disk cache");
                    return Ok(Stream {
                        body: Box::new(tokio::fs::File::from_std(file)),
                        format,
                        start_sample: 0,
                        live: false,
                    });
                }
            } else {
                inc(&self.0.metrics.audio_cache.miss);
            }
        }

        let key = playable.key();
        let mut stream = match self.0.heads.get(&key).filter(|_| start_ms == 0) {
            Some(head) => {
                inc(&self.0.metrics.prefetch.hit);
                let rest = (!head.complete).then(|| {
                    let src = src.clone();
                    let t = playable.clone();
                    async move {
                        let p = src.playable().expect("playable");
                        p.open(&t, 0).await.map(|s| s.body).map_err(std::io::Error::other)
                    }
                });
                Stream {
                    body: Box::new(HeadReader::new(head.data.clone(), rest)),
                    format: head.format.clone(),
                    start_sample: 0,
                    live: false,
                }
            }
            None => {
                if start_ms == 0 {
                    inc(&self.0.metrics.prefetch.miss);
                }
                player.open(&playable, start_ms).await?
            }
        };

        if let (Some(disk), true) = (
            &self.0.disk,
            cacheable && !stream.live && stream.start_sample == 0 && start_ms == 0,
        ) {
            let plays = disk.plays.get(&key).unwrap_or(0) + 1;
            disk.plays.set(key.clone(), plays);
            if plays >= PLAYS_TO_CACHE {
                let cache_key = format!("{}-{}", Self::disk_key(&playable), ext_for(&stream.format));
                match disk.store.create(&cache_key, 0) {
                    Ok(w) => {
                        let body: Body = std::mem::replace(&mut stream.body, Box::new(tokio::io::empty()));
                        stream.body = Box::new(TeeReader::new(body, w, self.0.metrics.clone(), cache_key));
                    }
                    Err(e) => {
                        inc(&self.0.metrics.audio_cache.write_failed);
                        tracing::warn!(track = %key, error = %e, "audio cache write could not start");
                    }
                }
            }
        }
        Ok(stream)
    }

    /// Queue `tracks` for prefetching; returns how many were accepted
    /// (already-pending duplicates count as accepted).
    pub fn prefetch(&self, tracks: &[Track]) -> usize {
        tracks.iter().filter(|t| self.enqueue((*t).clone())).count()
    }

    /// Queue one track; `false` when the pending queue is full.
    pub fn enqueue(&self, t: Track) -> bool {
        let key = t.key();
        {
            let mut w = self.0.warming.lock();
            if w.contains(&key) {
                return true;
            }
            if w.len() >= PREFETCH_PENDING {
                drop(w);
                inc(&self.0.metrics.prefetch.dropped);
                tracing::warn!(track = %key, pending = PREFETCH_PENDING, "prefetch dropped, queue full");
                return false;
            }
            w.insert(key.clone());
        }
        inc(&self.0.metrics.prefetch.queued);
        let me = self.clone();
        tokio::spawn(async move {
            let _permit = me.0.warm_slots.clone().acquire_owned().await;
            if tokio::time::timeout(PREFETCH_TIMEOUT, me.prefetch_one(&t))
                .await
                .is_err()
            {
                inc(&me.0.metrics.prefetch.failed);
                tracing::warn!(track = %key, "prefetch timed out");
            }
            me.0.warming.lock().remove(&key);
        });
        true
    }

    /// Mirror `t`, warm its source and keep the first [`HEAD_BYTES`] in memory.
    pub async fn prefetch_one(&self, t: &Track) {
        let playable = match self.playable(t).await {
            Ok(p) => p,
            Err(e) => {
                inc(&self.0.metrics.prefetch.failed);
                tracing::warn!(track = %t.key(), error = %e, "prefetch failed");
                return;
            }
        };
        let Some(src) = self.player_for(&playable) else { return };
        let p = src.playable().expect("playable");
        p.warm(&playable).await;
        let key = playable.key();
        if playable.is_stream || self.0.heads.contains(&key) {
            return;
        }
        if let Some(disk) = &self.0.disk {
            if disk
                .store
                .find_prefix(&format!("{}-", Self::disk_key(&playable)))
                .is_some()
            {
                return;
            }
        }
        let mut stream = match p.open(&playable, 0).await {
            Ok(s) => s,
            Err(e) => {
                inc(&self.0.metrics.prefetch.failed);
                tracing::warn!(track = %key, error = %e, "prefetch open failed");
                return;
            }
        };
        if stream.live || stream.start_sample != 0 {
            return;
        }
        let mut buf = BytesMut::with_capacity(HEAD_BYTES);
        let mut complete = false;
        while buf.len() < HEAD_BYTES {
            let mut chunk = vec![0u8; (HEAD_BYTES - buf.len()).min(64 * 1024)];
            match stream.body.read(&mut chunk).await {
                Ok(0) => {
                    complete = true;
                    break;
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    inc(&self.0.metrics.prefetch.failed);
                    tracing::warn!(track = %key, error = %e, "prefetch read failed");
                    return;
                }
            }
        }
        if buf.is_empty() {
            return;
        }
        tracing::debug!(track = %key, bytes = buf.len(), complete, "prefetched head");
        self.0.heads.set(
            key,
            Arc::new(Head {
                data: buf.freeze(),
                complete,
                format: stream.format.clone(),
            }),
        );
    }

    /// Recommendations after `t` (autoplay/radio). Uses `t`'s own source when
    /// it recommends, otherwise the first recommender, seeded by ISRC or a
    /// scored search. Fan edits are dropped.
    pub async fn recommend(&self, t: &Track, limit: usize) -> Result<Vec<Track>, Failure> {
        let own = self
            .0
            .by_name
            .get(&t.source)
            .filter(|s| s.recommender().is_some())
            .cloned();
        let (src, seed) = match own {
            Some(s) => (s, t.identifier.clone()),
            None => {
                let Some(s) = self.0.ordered.iter().find(|s| s.recommender().is_some()).cloned() else {
                    return Ok(vec![]);
                };
                let mut seed = None;
                if let (Some(l), false) = (s.isrc_lookup(), t.isrc.is_empty()) {
                    seed = l.by_isrc(&t.isrc).await.ok().map(|h| h.identifier);
                }
                if seed.is_none() {
                    let found = s
                        .search(format!("{} {}", t.title, t.author).trim())
                        .await
                        .unwrap_or_default();
                    seed = best_match(t, &found).map(|h| h.identifier);
                }
                match seed {
                    Some(seed) => (s, seed),
                    None => {
                        inc(&self.0.metrics.recommend.empty);
                        return Ok(vec![]);
                    }
                }
            }
        };
        let r = src.recommender().expect("checked");
        match r.recommend(&seed, limit).await {
            Ok(found) => {
                let out: Vec<Track> = found.into_iter().filter(|x| !fan_edit(x)).take(limit).collect();
                inc(if out.is_empty() {
                    &self.0.metrics.recommend.empty
                } else {
                    &self.0.metrics.recommend.served
                });
                Ok(out)
            }
            Err(e) => {
                inc(&self.0.metrics.recommend.failed);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wavyzon_plugin::{IsrcLookup, Playable, Recommender};

    fn tr(source: &str, id: &str, title: &str, author: &str, dur: i64) -> Track {
        Track {
            source: source.into(),
            identifier: id.into(),
            title: title.into(),
            author: author.into(),
            duration_ms: dur,
            ..Default::default()
        }
    }

    /// A playable source with ISRC lookup and canned search results.
    #[derive(Default)]
    struct FakeAudio {
        name: &'static str,
        isrc: HashMap<String, Track>,
        results: Mutex<Vec<Track>>,
        searches: AtomicU32,
        opens: AtomicU32,
        body: &'static [u8],
        isrc_capable: bool,
        fail_search: bool,
        no_text_search: bool,
    }

    #[async_trait]
    impl Source for FakeAudio {
        fn name(&self) -> &str {
            self.name
        }
        fn search_prefix(&self) -> Option<&str> {
            Some(if self.name == "fake" { "fksearch" } else { "fbsearch" })
        }
        fn searches_text(&self) -> bool {
            !self.no_text_search
        }
        async fn search(&self, _q: &str) -> Result<Vec<Track>, Failure> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            if self.fail_search {
                return Err(Failure::common(self.name, "search down", None));
            }
            Ok(self.results.lock().clone())
        }
        async fn track(&self, _id: &str) -> Result<Track, Failure> {
            Err(Failure::common(self.name, "no", None))
        }
        fn playable(&self) -> Option<&dyn Playable> {
            Some(self)
        }
        fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
            if self.isrc_capable { Some(self) } else { None }
        }
    }

    #[async_trait]
    impl Playable for FakeAudio {
        async fn open(&self, _t: &Track, _start: i64) -> Result<Stream, Failure> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Stream {
                body: Box::new(self.body),
                format: Format::mp3(),
                start_sample: 0,
                live: false,
            })
        }
    }

    #[async_trait]
    impl IsrcLookup for FakeAudio {
        async fn by_isrc(&self, isrc: &str) -> Result<Track, Failure> {
            self.isrc
                .get(isrc)
                .cloned()
                .ok_or_else(|| Failure::common(self.name, "not found", None))
        }
    }

    /// A metadata-only source.
    struct FakeMirror {
        full: Track,
        recs: Option<Vec<Track>>,
    }

    #[async_trait]
    impl Source for FakeMirror {
        fn name(&self) -> &str {
            "mirror"
        }
        fn search_prefix(&self) -> Option<&str> {
            Some("mrsearch")
        }
        async fn search(&self, _q: &str) -> Result<Vec<Track>, Failure> {
            Ok(vec![self.full.clone()])
        }
        fn matches_url(&self, url: &str) -> bool {
            url == "https://mirror/track"
        }
        async fn load_url(&self, _u: &str) -> Result<LoadResult, Failure> {
            Ok(LoadResult::single(self.full.clone()))
        }
        async fn track(&self, _id: &str) -> Result<Track, Failure> {
            Ok(self.full.clone())
        }
        fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> {
            Some(self)
        }
        fn recommender(&self) -> Option<&dyn Recommender> {
            self.recs.as_ref().map(|_| self as &dyn Recommender)
        }
    }

    #[async_trait]
    impl IsrcLookup for FakeMirror {
        async fn by_isrc(&self, _isrc: &str) -> Result<Track, Failure> {
            Ok(self.full.clone())
        }
    }

    #[async_trait]
    impl Recommender for FakeMirror {
        async fn recommend(&self, id: &str, limit: usize) -> Result<Vec<Track>, Failure> {
            assert!(
                limit == RECOMMEND_LIMIT || id != "110956532",
                "the rec: prefix asks for RECOMMEND_LIMIT"
            );
            if id == "boom" {
                return Err(Failure::common("mirror", "boom", None));
            }
            Ok(self.recs.clone().unwrap())
        }
    }

    fn audio(isrc: &[(&str, Track)], results: Vec<Track>) -> Arc<FakeAudio> {
        Arc::new(FakeAudio {
            name: "fake",
            isrc: isrc.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            results: Mutex::new(results),
            body: b"0123456789",
            isrc_capable: true,
            ..Default::default()
        })
    }

    fn mirror_src() -> Arc<FakeMirror> {
        let mut full = tr("mirror", "m1", "Song", "Artist", 181_000);
        full.isrc = "GB-AAA-00-00001".into();
        Arc::new(FakeMirror { full, recs: None })
    }

    fn build(a: Arc<FakeAudio>, extra: Vec<Arc<dyn Source>>, order: &[&str]) -> Sources {
        let mut b = SourcesBuilder::bare(
            order.iter().map(|s| s.to_string()).collect(),
            vec!["fake".into(), "fb".into()],
        )
        .builtin(a);
        for e in extra {
            b = b.builtin(e);
        }
        b.build()
    }

    #[tokio::test]
    async fn mirrors_by_isrc_and_caches() {
        let hit = Track {
            isrc: "GBAAA0000001".into(),
            ..tr("fake", "42", "Song", "Artist", 180_000)
        };
        let a = audio(&[("GBAAA0000001", hit)], vec![]);
        let r = build(a.clone(), vec![mirror_src()], &["mirror", "fake"]);
        let partial = tr("mirror", "m1", "Song", "Artist", 0);
        assert_eq!(r.playable(&partial).await.unwrap().identifier, "42");
        r.playable(&partial).await.unwrap();
        assert_eq!(
            a.searches.load(Ordering::SeqCst),
            0,
            "text search used despite an ISRC hit"
        );
        assert_eq!(r.metrics().mirror.cached.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn text_fallback_and_cached_misses() {
        let a = audio(&[], vec![tr("fake", "7", "Other Song", "Nobody", 100_000)]);
        let r = build(a.clone(), vec![], &[]);
        let target = tr("mirror", "x", "Song", "Artist", 180_000);
        assert!(r.playable(&target).await.is_err(), "matched an unrelated song");
        assert!(r.playable(&target).await.is_err());
        assert_eq!(a.searches.load(Ordering::SeqCst), 1, "miss not cached");
    }

    #[tokio::test]
    async fn concurrent_mirrors_are_deduplicated() {
        let a = audio(&[], vec![tr("fake", "1", "Song", "Artist", 180_000)]);
        let r = build(a.clone(), vec![], &[]);
        let target = tr("spotify", "s", "Song", "Artist", 180_500);
        let futs: Vec<_> = (0..8).map(|_| r.playable(&target)).collect();
        for res in futures::future::join_all(futs).await {
            assert_eq!(res.unwrap().identifier, "1");
        }
        assert_eq!(a.searches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn routes_identifiers() {
        let hit = tr("fake", "42", "Song", "", 1000);
        let a = audio(&[("GBAAA0000001", hit.clone())], vec![hit]);
        let r = build(a, vec![mirror_src()], &["mirror", "fake"]);
        let res = r.load("dzisrc:GB-AAA-00-00001").await;
        assert_eq!(
            (res.kind, res.tracks[0].identifier.as_str()),
            (LoadType::Track, "42"),
            "playback sources answer ISRC first"
        );
        assert_eq!(r.load("https://mirror/track").await.tracks[0].source, "mirror");
        let s = r.load("song").await;
        assert_eq!((s.kind, s.tracks[0].source.as_str()), (LoadType::Search, "mirror"));
        assert_eq!(r.load("fksearch:song").await.tracks[0].source, "fake");
        assert_eq!(r.load("https://unknown.example/x").await.kind, LoadType::Error);
        assert_eq!(
            r.load("ytsearch: song").await.tracks[0].source,
            "mirror",
            "youtube aliases are plain searches"
        );
        assert_eq!(r.load("  ").await.kind, LoadType::Empty);
    }

    #[tokio::test]
    async fn plugins_are_consulted_first_and_can_replace_builtins() {
        let a = audio(&[], vec![tr("fake", "1", "Song", "", 0)]);
        let plugin = Arc::new(FakeMirror {
            full: tr("mirror", "plugin", "Song", "", 0),
            recs: None,
        });
        let r = SourcesBuilder::bare(vec![], vec!["fake".into()])
            .builtin(a)
            .builtin(mirror_src())
            .plugin(plugin)
            .build();
        assert_eq!(r.load("https://mirror/track").await.tracks[0].identifier, "plugin");
        assert_eq!(r.names(), ["mirror", "fake"]);
    }

    #[tokio::test]
    async fn fallback_answers_only_when_the_primary_has_nothing() {
        let primary = audio(&[], vec![]);
        let fb = Arc::new(FakeAudio {
            name: "fb",
            results: Mutex::new(vec![tr("fb", "9", "Rare Song", "Indie", 200_000)]),
            body: b"fb",
            ..Default::default()
        });
        let r = build(primary.clone(), vec![fb.clone()], &[]);
        assert_eq!(r.playback_names(), ["fake", "fb"]);
        let res = r.load("fksearch:rare song").await;
        assert_eq!(res.tracks[0].source, "fb");
        let got = r
            .playable(&tr("mirror", "s1", "Rare Song", "Indie", 201_000))
            .await
            .unwrap();
        assert_eq!(got.source, "fb");
        r.open(&got, 0).await.unwrap();
        assert_eq!(fb.opens.load(Ordering::SeqCst), 1);
        assert_eq!(r.metrics().fallback.mirror.load(Ordering::SeqCst), 1);

        *primary.results.lock() = vec![tr("fake", "1", "Common Song", "Band", 180_000)];
        assert_eq!(r.load("fksearch:common song").await.tracks[0].source, "fake");
        let m = r
            .playable(&tr("mirror", "s2", "Common Song", "Band", 181_000))
            .await
            .unwrap();
        assert_eq!(m.source, "fake", "primary should win the mirror");
    }

    /// The primary searched fine and found nothing; a failing fallback does
    /// not turn that into an error (Cadence returns only the primary's
    /// error), and the miss is cached.
    #[tokio::test]
    async fn failing_fallback_after_an_empty_primary_is_a_cached_miss() {
        let primary = audio(&[], vec![]);
        let fb = Arc::new(FakeAudio {
            name: "fb",
            fail_search: true,
            ..Default::default()
        });
        let r = build(primary.clone(), vec![fb.clone()], &[]);
        let t = tr("mirror", "s1", "Nowhere Song", "Nobody", 200_000);
        let err = r.playable(&t).await.unwrap_err();
        assert!(err.message.contains("no matching recording"), "{err:?}");
        assert_eq!(r.metrics().mirror.miss.load(Ordering::SeqCst), 1);
        r.playable(&t).await.unwrap_err();
        assert_eq!(primary.searches.load(Ordering::SeqCst), 1, "the miss was not cached");
    }

    /// A source whose search only decodes identifiers (`tone`) is never
    /// asked to stand in for a text search: it would only fail.
    #[tokio::test]
    async fn fallback_skips_sources_that_do_not_search_text() {
        let primary = audio(&[], vec![]);
        let tone = Arc::new(FakeAudio {
            name: "tn",
            fail_search: true,
            no_text_search: true,
            ..Default::default()
        });
        let fb = Arc::new(FakeAudio {
            name: "fb",
            results: Mutex::new(vec![tr("fb", "9", "Rare Song", "Indie", 200_000)]),
            body: b"fb",
            ..Default::default()
        });
        let r = build(primary.clone(), vec![tone.clone(), fb.clone()], &[]);
        let res = r.load("fksearch:rare song").await;
        assert_eq!(res.tracks[0].source, "fb");
        assert_eq!(tone.searches.load(Ordering::SeqCst), 0, "tone was asked a title");
    }

    #[tokio::test]
    async fn recommend_prefix_and_fan_edits() {
        let recs = vec![
            tr("mirror", "1", "Gilded Lily", "", 0),
            tr("mirror", "2", "Gilded Lily (Sped Up)", "", 0),
        ];
        let m = Arc::new(FakeMirror {
            full: tr("mirror", "m", "x", "", 0),
            recs: Some(recs),
        });
        let r = build(audio(&[], vec![]), vec![m], &[]);
        let res = r.load("mrrec: 110956532 ").await;
        assert_eq!(res.kind, LoadType::Search);
        assert_eq!(res.tracks.len(), 1);
        assert_eq!(res.tracks[0].identifier, "1");
        assert_eq!(r.load("mrrec:boom").await.kind, LoadType::Error);
        assert_eq!(
            r.load("fkrec:1").await.kind,
            LoadType::Empty,
            "a prefix without a recommender is a plain search (empty order here)"
        );
        let own = r.recommend(&tr("mirror", "seed", "", "", 0), 10).await.unwrap();
        assert_eq!(own.len(), 1);
        let via_isrc = r
            .recommend(
                &Track {
                    isrc: "GBAAA0000001".into(),
                    ..tr("fake", "x", "Song", "Artist", 0)
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(via_isrc.len(), 1);
    }

    #[tokio::test]
    async fn enqueue_is_bounded_and_deduplicated() {
        let r = build(audio(&[], vec![]), vec![], &[]);
        {
            let mut w = r.0.warming.lock();
            for i in 0..PREFETCH_PENDING {
                w.insert(format!("k{i}"));
            }
        }
        assert!(
            !r.enqueue(tr("fake", "new", "", "", 0)),
            "enqueue accepted past the pending bound"
        );
        *r.0.warming.lock() = HashSet::from(["fake:dup".to_string()]);
        assert!(r.enqueue(tr("fake", "dup", "", "", 0)));
        assert_eq!(r.0.warming.lock().len(), 1);
        assert_eq!(r.metrics().prefetch.dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prefetched_heads_serve_the_start_and_join_the_rest() {
        let a = audio(&[], vec![]);
        let r = build(a.clone(), vec![], &[]);
        let t = tr("fake", "1", "S", "A", 1000);
        r.prefetch_one(&t).await;
        assert_eq!(a.opens.load(Ordering::SeqCst), 1);
        let mut s = r.open(&t, 0).await.unwrap();
        let mut out = Vec::new();
        s.body.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"0123456789");
        assert_eq!(
            a.opens.load(Ordering::SeqCst),
            1,
            "a complete head needs no second open"
        );
        assert_eq!(r.metrics().prefetch.hit.load(Ordering::SeqCst), 1);
        r.open(&tr("fake", "2", "S", "A", 1000), 0).await.unwrap();
        assert_eq!(r.metrics().prefetch.miss.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn second_play_is_cached_on_disk_and_then_served_from_it() {
        let d = TempDir::new();
        let metrics = Arc::new(Metrics::default());
        let store = Arc::new(Store::open(d.path(), 1 << 20, metrics.clone()).unwrap());
        let a = audio(&[], vec![]);
        let r = SourcesBuilder::bare(vec![], vec!["fake".into()])
            .metrics(metrics.clone())
            .builtin(a.clone())
            .audio_cache(store.clone())
            .build();
        let t = tr("fake", "1", "S", "A", 1000);
        for _ in 0..2 {
            let mut s = r.open(&t, 0).await.unwrap();
            let mut out = Vec::new();
            s.body.read_to_end(&mut out).await.unwrap();
        }
        assert_eq!(store.stats().0, 1, "second play must be stored");
        let mut s = r.open(&t, 0).await.unwrap();
        assert_eq!(s.format, Format::mp3());
        let mut out = Vec::new();
        s.body.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"0123456789");
        assert_eq!(a.opens.load(Ordering::SeqCst), 2, "third play comes from disk");
        assert_eq!(metrics.audio_cache.hit.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.audio_cache.stored.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_playback_source_is_a_fault() {
        let r = SourcesBuilder::bare(vec![], vec![]).builtin(mirror_src()).build();
        let err = r.playable(&tr("mirror", "x", "a", "b", 0)).await.unwrap_err();
        assert_eq!(err.severity, wavyzon_plugin::Severity::Fault);
        let enc = tr("mirror", "x", "a", "b", 0).encode();
        assert_eq!(r.decode(&enc).unwrap().identifier, "x");
    }

    /// A lyrics provider with a fixed answer: `Some(text)`, `None`, or failing.
    struct FakeLyrics(&'static str, Option<Option<&'static str>>, AtomicU32);

    #[async_trait]
    impl LyricsProvider for FakeLyrics {
        fn name(&self) -> &str {
            self.0
        }
        async fn lyrics(&self, _: &Track) -> Result<Option<Lyrics>, Failure> {
            self.2.fetch_add(1, Ordering::SeqCst);
            match self.1 {
                None => Err(Failure::common(self.0, "down", None)),
                Some(found) => Ok(found.map(|t| Lyrics {
                    text: t.into(),
                    provider: self.0.to_string().leak(),
                })),
            }
        }
    }

    fn chain(providers: &[&Arc<FakeLyrics>]) -> Sources {
        SourcesBuilder::bare(vec![], vec![])
            .lyrics_providers(providers.iter().map(|p| (*p).clone() as Arc<dyn LyricsProvider>))
            .build()
    }

    #[tokio::test]
    async fn lyrics_ask_providers_in_order_until_one_has_them() {
        let t = tr("x", "1", "Song", "Artist", 0);
        let genius = |a| Arc::new(FakeLyrics("genius", a, AtomicU32::new(0)));
        let ovh = |a| Arc::new(FakeLyrics("ovh", a, AtomicU32::new(0)));

        assert!(!chain(&[]).lyrics_enabled());

        // First provider has them: the next one is never asked.
        let (g, o) = (genius(Some(Some("from genius"))), ovh(Some(Some("from ovh"))));
        let s = chain(&[&g, &o]);
        assert_eq!(s.lyrics_providers(), ["genius", "ovh"]);
        assert_eq!(s.lyrics(&t).await.unwrap().unwrap().provider, "genius");
        assert_eq!(o.2.load(Ordering::SeqCst), 0);
        assert_eq!(s.metrics().lyrics_plugins.found.load(Ordering::Relaxed), 1);

        // None, or a failure, falls through to the next one.
        for first in [Some(None), None] {
            let (g, o) = (genius(first), ovh(Some(Some("from ovh"))));
            assert_eq!(chain(&[&g, &o]).lyrics(&t).await.unwrap().unwrap().provider, "ovh");
        }

        // One failed, the other has none: no lyrics (204), not an error.
        let (g, o) = (genius(None), ovh(Some(None)));
        assert_eq!(chain(&[&g, &o]).lyrics(&t).await.unwrap(), None);
        // Every provider failed: the error.
        let (g, o) = (genius(None), ovh(None));
        assert!(chain(&[&g, &o]).lyrics(&t).await.is_err());
    }
}
