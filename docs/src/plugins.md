# Writing plugins

Wavyzon can be extended without forking it. A plugin is an ordinary **Rust
crate** that depends on [`wavyzon-plugin`](https://gitlab.com/ihrz/wavyzon/-/tree/main/crates/wavyzon-plugin)
and implements one trait, `Plugin`. It can add:

| Extension | Trait(s) | Shows up as |
| --- | --- | --- |
| [Sources](#2-sources) | `Source` (+ `Playable`, `IsrcLookup`, `Recommender`) | `loadtracks` prefixes and links, playback, mirroring targets |
| [Audio filters](#3-audio-filters) | `FilterFactory` → `AudioFilter` | `filters.pluginFilters.<name>` |
| [Event listeners](#4-event-listeners) | `EventListener` | every player event of every session |
| [HTTP routes](#5-http-routes) | an `axum::Router` | `/v1/plugins/<name>/…` |

The complete, tested example used throughout this page lives in
[`plugins/wavyzon-plugin-example`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-example).

Three real plugins ship with the node and are on by default, each also a
model to copy:

| Plugin | Adds | Docs |
| --- | --- | --- |
| [`apo`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-apo) | a filter (`pluginFilters.apo`) and routes | [Equalizer APO](apo.md) |
| [`flowery`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-flowery) | a playable source for `ftts://` links, streamed with `wavyzon-sources`' shared `HttpClient` | [Flowery TTS](flowery-tts.md) |
| [`genius`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-genius) | a lyrics provider asked before lyrics.ovh | [Genius lyrics](genius-lyrics.md) |

## How plugins are loaded

Plugins are **compiled in**, not loaded from shared libraries at runtime:

```text
          compile time                                    runtime
┌──────────────────────────────────┐        ┌─────────────────────────────────────────┐
│ cargo build -p wavyzon           │        │ WAVYZON_PLUGINS=example,other           │
│   --features plugin-example      │ ─────► │ WAVYZON_PLUGIN_EXAMPLE_AMPLITUDE=0.3    │
│ (links the plugin crate)         │        │ → manifest check → register() → serve   │
└──────────────────────────────────┘        └─────────────────────────────────────────┘
```

1. A **cargo feature** of the `wavyzon` binary links the plugin crate.
2. At startup, the node instantiates the compiled-in plugins named in
   `WAVYZON_PLUGINS`, in that order (all compiled-in plugins when it is empty).
3. For each, it reads the [`PluginManifest`](#the-manifest) and refuses a
   plugin built for another API major ([versioning](#versioning)).
4. It builds a `PluginContext` (settings from `WAVYZON_PLUGIN_<NAME>_*`, a
   private data directory) and calls `register`, where the plugin adds its
   extensions to the `Registry`.
5. Only then does the server start listening. On graceful shutdown each
   plugin's `shutdown` is called.

Why compile-time? Rust has no stable ABI, so dynamically loaded plugins would
have to match the node's exact compiler and dependency versions anyway, and
would bring `unsafe` FFI. Compiled-in plugins are type-checked against the
node, run at full speed on the real-time audio path, and keep the "one static
binary" deployment.

## 1. Create the crate

Outside the repository:

```bash
cargo new --lib wavyzon-plugin-hello
cd wavyzon-plugin-hello
```

```toml
# Cargo.toml
[package]
name = "wavyzon-plugin-hello"
version = "0.1.0"
edition = "2024"

[dependencies]
wavyzon-plugin = { git = "https://gitlab.com/ihrz/wavyzon", tag = "v0.1.0" }
tokio = { version = "1", features = ["io-util"] }   # only if you implement Playable
tracing = "0.1"                                        # optional, logs go to the node's subscriber
```

Inside the Wavyzon workspace (like the bundled example), use workspace
inheritance instead:

```toml
[package]
name = "wavyzon-plugin-example"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
publish = false

[dependencies]
wavyzon-plugin.workspace = true
tokio.workspace = true
tracing.workspace = true
```

`wavyzon-plugin` re-exports `async_trait`, `axum` and `serde_json`. **Use the
re-exports** (`wavyzon_plugin::axum::Router`) rather than your own versions,
so your routes and filter configs are the exact types the node expects.

### The smallest plugin

```rust
use std::sync::Arc;
use wavyzon_plugin::*;

pub struct Hello;

#[async_trait]
impl Plugin for Hello {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: "hello",
            version: env!("CARGO_PKG_VERSION"),
            description: "Says hi",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let greeting = ctx.setting("greeting").unwrap_or("hi").to_string();
        reg.routes(axum::Router::new().route("/", axum::routing::get(move || async move { greeting })));
        Ok(())
    }
}

/// Entry point used by the node.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(Hello)
}
```

```bash
WAVYZON_PLUGINS=hello WAVYZON_PLUGIN_HELLO_GREETING=bonjour ./wavyzon
curl -H "Authorization: $WAVYZON_PASSWORD" localhost:2333/v1/plugins/hello/
# bonjour
```

### The manifest

| Field | Rules |
| --- | --- |
| `name` | Lowercase, URL-safe, unique (`"hello"`, `"my-lyrics"`). It is the route prefix `/v1/plugins/<name>`, the `WAVYZON_PLUGINS` entry and the env prefix `WAVYZON_PLUGIN_<NAME>_` (upper-cased, `-` → `_`) |
| `version` | Your plugin's version; use `env!("CARGO_PKG_VERSION")` |
| `description` | One line, shown in `/v1/plugins` and `/v1/info` |
| `api_version` | Always `API_VERSION`, the constant of the `wavyzon-plugin` you compiled against |

### Settings and data

```rust
async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
    // WAVYZON_PLUGIN_HELLO_API_KEY=xyz  →  ctx.setting("api_key") / ctx.setting("API_KEY")
    let key = ctx.setting("api_key").ok_or_else(|| PluginError::Config("API_KEY is required".into()))?;

    // A private, writable directory: $WAVYZON_DATA_DIR/plugins/hello (create it when needed)
    std::fs::create_dir_all(&ctx.data_dir).map_err(|e| PluginError::Other(e.to_string()))?;
    let db = ctx.data_dir.join("cache.json");

    tracing::info!(node = ctx.node_version, "hello plugin ready");
    Ok(())
}
```

- `ctx.settings` holds every `WAVYZON_PLUGIN_<NAME>_*` variable, keyed by the
  **lower-cased** suffix. `ctx.setting(key)` lower-cases its argument for you.
- Return `PluginError::Config` for bad settings: the node refuses to start
  and prints your message, which is better than failing on the first request.
- `ctx.data_dir` is *not* created for you.

### Lifecycle rules

- `register` runs **once**, before the server listens, in `WAVYZON_PLUGINS`
  order. It may be `async` (connect to a database, warm a cache), but keep it
  short: the node is not serving yet.
- Everything you register must be `Send + Sync + 'static`; share state with
  `Arc`.
- `shutdown` runs once on graceful shutdown (SIGTERM / Ctrl-C). Flush and
  close there.

## 2. Sources

A source resolves queries and links to `Track`s. A **playable** source can
also stream them.

```rust
#[async_trait]
pub trait Source: Send + Sync + 'static {
    fn name(&self) -> &str;                                       // stored in Track::source
    fn search_prefix(&self) -> Option<&str> { None }              // "tone" → "tone:…" queries
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> { Ok(vec![]) }
    fn searches_text(&self) -> bool { true }                      // false: search only decodes ids
    fn matches_url(&self, url: &str) -> bool { false }            // claim links
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure>;
    async fn track(&self, identifier: &str) -> Result<Track, Failure>;   // required
    fn playable(&self) -> Option<&dyn Playable> { None }
    fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> { None }
    fn recommender(&self) -> Option<&dyn Recommender> { None }
}
```

How the node uses it:

| Client sends | Node calls |
| --- | --- |
| `loadtracks?identifier=<prefix>:<query>` | `search(query)` → `loadType: "search"` (or `empty`) |
| `loadtracks?identifier=<url>` where `matches_url(url)` | `load_url(url)` → whatever `LoadResult` you return. Plugin sources are asked **before** built-in ones |
| A player update with one of your tracks | `playable().open(track, start_ms)` |
| Mirroring a Spotify/Apple/Deezer track, if your source is in `WAVYZON_PLAYBACK_SOURCES` | `isrc_lookup().by_isrc(isrc)` |
| A text search another playback source found nothing for, if your source is in `WAVYZON_PLAYBACK_SOURCES` and `searches_text()` is `true` | `search(query)` as a fallback. Return `false` when your search only decodes an identifier (like `tone:440`): a song title would only fail there |

### Tracks

```rust
Track {
    source: "tone".into(),                // = Source::name()
    identifier: "440:30".into(),          // enough for track()/open() to find it again
    title: "Sine 440 Hz".into(),
    author: "wavyzon-plugin-example".into(),
    duration_ms: 30_000,                  // 0 if unknown
    uri: "tone://440:30".into(),          // optional
    is_stream: false,                     // true for endless audio
    ..Default::default()                  // album, isrc, artwork_url
}
```

A track is encoded (base64url JSON) and sent to the client, which sends it
back later, possibly **to another node or after a restart**. So `identifier`
must be self-sufficient: don't make it an index into in-memory state.

Use `LoadResult::single`, `::results` (search), `::playlist`, `::empty` and
`::error` to build results.

### Failures

Return a `Failure` with the right severity; it reaches the client as
`loadType: "error"` or a `TrackExceptionEvent`, and is counted in metrics.

```rust
Failure::common("tone", "invalid tone", Some("frequency must be 20-20000 Hz".into()))   // expected
Failure::suspicious("mysource", "refused by source", Some("HTTP 403".into()))            // rate-limited / banned
Failure::fault("mysource", "unexpected response", Some(err.to_string()))                 // a bug
Failure::from_status("mysource", status, body)    // 401/403/429 → suspicious, 404 → common, else common
```

### Playback

```rust
#[async_trait]
pub trait Playable: Send + Sync {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure>;
    fn seekable(&self) -> bool { false }
    async fn warm(&self, track: &Track) {}
}

pub struct Stream {
    pub body: Box<dyn AsyncRead + Send + Unpin>,
    pub format: Format,          // Pcm48 or Encoded { hint }
    pub start_sample: i64,       // 48 kHz sample where body starts
    pub live: bool,              // endless (radio)
}
```

- **`Format::Encoded { hint }`**: bytes of an MP3, AAC/M4A, FLAC, Ogg Vorbis
  or WAV file; Symphonia decodes them. `hint` (`"mp3"`, `"audio/ogg"`) speeds
  up probing. This is what you want for anything fetched from the web.
- **`Format::Pcm48`**: raw interleaved **signed 16-bit little-endian stereo
  at 48 kHz** (4 bytes per sample frame). Use it for audio you synthesise or
  decode yourself.
- **Seeking.** If you can start at `start_ms` cheaply (Range request,
  generated audio), do it, set `start_sample = start_ms * 48` and make
  `seekable()` return `true`. Otherwise ignore `start_ms`, leave
  `start_sample = 0`, and the player decodes from the start and skips
  forward; correct, just slower.
- **`warm`** is a hint that the track will probably play soon (it is in a
  prefetch list or `nextTrack`): resolve stream URLs, open connections.
- `body` is read on a decode task, never on the send loop, so a slow read
  causes buffering (and eventually `TrackStuckEvent`), not stutter.

### Full example: a sine-tone source

From the example plugin (`src/tone.rs`), abridged. It serves `tone:440`,
`tone:440:30` and `tone:440:live`, as PCM.

```rust
use std::{io, pin::Pin, sync::Arc, task::{Context, Poll}};
use tokio::io::{AsyncRead, ReadBuf};
use wavyzon_plugin::*;

pub struct ToneSource { settings: ToneSettings, shared: Arc<Shared> }

#[async_trait]
impl Source for ToneSource {
    fn name(&self) -> &str { "tone" }
    fn search_prefix(&self) -> Option<&str> { Some("tone") }

    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        Ok(vec![ToneSpec::parse(query, &self.settings)?.to_track()])
    }
    // A frequency, not a title: never used as a text-search fallback.
    fn searches_text(&self) -> bool { false }
    fn matches_url(&self, url: &str) -> bool { url.starts_with("tone://") }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let rest = url.strip_prefix("tone://").unwrap_or(url);
        Ok(LoadResult::single(ToneSpec::parse(rest, &self.settings)?.to_track()))
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        Ok(ToneSpec::parse(identifier, &self.settings)?.to_track())
    }
    fn playable(&self) -> Option<&dyn Playable> { Some(self) }
}

#[async_trait]
impl Playable for ToneSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let spec = ToneSpec::parse(&track.identifier, &self.settings)?;
        let start = start_ms.max(0) * SAMPLE_RATE as i64 / 1000;
        Ok(Stream {
            body: Box::new(ToneReader::new(spec, self.settings.amplitude, start)),
            format: Format::Pcm48,
            start_sample: start,           // we honoured the seek
            live: spec.seconds.is_none(),
        })
    }
    fn seekable(&self) -> bool { true }
}

/// Generates s16le stereo frames on demand.
pub struct ToneReader { step: f64, amplitude: f64, pos: i64, end: Option<i64>, pending: [u8; 4], pending_at: usize }

impl AsyncRead for ToneReader {
    fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        while buf.remaining() > 0 {
            if self.pending_at == 4 && !self.next_frame() {
                break; // EOF
            }
            let at = self.pending_at;
            let n = (4 - at).min(buf.remaining());
            buf.put_slice(&self.pending[at..at + n]);
            self.pending_at += n;
        }
        Poll::Ready(Ok(()))
    }
}
```

Try it:

```bash
curl -s -H "Authorization: $WAVYZON_PASSWORD" "localhost:2333/v1/loadtracks?identifier=tone:440:5" | jq '.data[0].encoded'
```

### ISRC lookup and recommendations

```rust
#[async_trait]
impl IsrcLookup for MySource {
    async fn by_isrc(&self, isrc: &str) -> Result<Track, Failure> {
        self.api.track_by_isrc(isrc).await.map_err(|e| Failure::common(self.name(), "isrc lookup failed", Some(e.to_string())))
    }
}

#[async_trait]
impl Recommender for MySource {
    async fn recommend(&self, identifier: &str, limit: usize) -> Result<Vec<Track>, Failure> {
        self.api.related(identifier, limit).await.map_err(|e| Failure::common(self.name(), "no recommendations", Some(e.to_string())))
    }
}

// and in the Source impl:
fn isrc_lookup(&self) -> Option<&dyn IsrcLookup> { Some(self) }
fn recommender(&self) -> Option<&dyn Recommender> { Some(self) }
```

A source with `IsrcLookup` + `Playable`, listed in `WAVYZON_PLAYBACK_SOURCES`,
becomes a mirroring target for Spotify, Apple Music and Deezer tracks.

> Note: provisional. The REST route that exposes `Recommender` (radio /
> autoplay) is not final.

## 3. Audio filters

```rust
pub trait FilterFactory: Send + Sync + 'static {
    fn name(&self) -> &str;                                              // key under pluginFilters
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure>;
}

pub trait AudioFilter: Send {
    fn process(&mut self, frame: &mut [f32]);                            // every 20 ms
    fn reconfigure(&mut self, config: &Value) -> bool { false }
    fn reset(&mut self) {}
}
```

- The factory is shared; `create` builds **one filter instance per deck** of
  each player (two while a crossfade runs). It is also called once to
  validate a new configuration before it is applied.
- `create` receives the client's JSON from `filters.pluginFilters.<name>`.
  Validate it; an `Err` is returned to the client as `400` with your message.
- `process` gets one 20 ms frame: `FRAME_SAMPLES` (1 920) interleaved stereo
  `f32` samples (`L R L R …`) at `SAMPLE_RATE` (48 000), values in
  `-1.0..=1.0`. Modify them in place.
- `process` runs on the **real-time send path**. It must not block, lock
  contended mutexes, do I/O or allocate per frame. Allocate buffers in
  `create`.
- `reconfigure` is called when the client changes the config of a running
  filter. Apply it and return `true` to keep your state (no click); return
  `false` to have the node build a new instance through `create` and
  crossfade from the old one to it.
- Filters added or removed on a running player fade in and out (the node
  blends your output with the dry signal), so you don't need to ramp
  yourself.
- `reset` is called on seek and on track change: clear delay lines, held
  samples, envelopes.

### Full example: bitcrush

From `src/bitcrush.rs` in the example plugin:

```rust
use wavyzon_plugin::serde_json::Value;
use wavyzon_plugin::{AudioFilter, CHANNELS, Failure, FilterFactory};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BitcrushConfig { pub bits: u32, pub downsample: u32, pub mix: f32 }

impl BitcrushConfig {
    pub fn parse(config: &Value) -> Result<Self, Failure> {
        let bad = |msg: &str| Failure::common("bitcrush", "invalid bitcrush filter", Some(msg.to_string()));
        let mut c = Self { bits: 8, downsample: 1, mix: 1.0 };
        let Value::Object(obj) = config else { return Err(bad("expected an object")) };
        if let Some(v) = obj.get("bits") {
            let b = v.as_u64().filter(|b| (1..=16).contains(b)).ok_or_else(|| bad("bits must be 1-16"))?;
            c.bits = b as u32;
        }
        // … downsample (1-64), mix (0-1) …
        Ok(c)
    }
}

pub struct Bitcrush { cfg: BitcrushConfig, steps: f32, held: [f32; CHANNELS], countdown: u32 }

impl AudioFilter for Bitcrush {
    fn process(&mut self, frame: &mut [f32]) {
        let (wet, dry) = (self.cfg.mix, 1.0 - self.cfg.mix);
        for pair in frame.chunks_exact_mut(CHANNELS) {
            if self.countdown == 0 {
                for (ch, s) in pair.iter().enumerate() {
                    self.held[ch] = ((*s * self.steps).round() / self.steps).clamp(-1.0, 1.0);
                }
                self.countdown = self.cfg.downsample;
            }
            self.countdown -= 1;
            for (ch, s) in pair.iter_mut().enumerate() {
                *s = *s * dry + self.held[ch] * wet;
            }
        }
    }

    fn reconfigure(&mut self, config: &Value) -> bool {
        match BitcrushConfig::parse(config) {
            Ok(cfg) => { self.cfg = cfg; self.steps = (1u32 << (cfg.bits - 1)) as f32; true }
            Err(_) => false, // the node calls create(), which reports the error
        }
    }

    fn reset(&mut self) { self.held = [0.0; CHANNELS]; self.countdown = 0; }
}

pub struct BitcrushFactory;

impl FilterFactory for BitcrushFactory {
    fn name(&self) -> &str { "bitcrush" }
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        let cfg = BitcrushConfig::parse(config)?;
        Ok(Box::new(Bitcrush { cfg, steps: (1u32 << (cfg.bits - 1)) as f32, held: [0.0; CHANNELS], countdown: 0 }))
    }
}
```

Client side:

```json
PATCH /v1/sessions/{session}/players/{guild}
{ "filters": { "pluginFilters": { "bitcrush": { "bits": 6, "downsample": 4 } } } }
```

## 4. Event listeners

```rust
#[async_trait]
pub trait EventListener: Send + Sync + 'static {
    async fn on_event(&self, session_id: &str, event: &Event);
}
```

- Called for **every** player event of every session (all the events of
  [WebSocket & events](api/websocket.md), including on v4 sessions that don't
  receive the extras), after the event was sent to the client.
- Listeners run on a dedicated task: a slow listener only delays itself,
  never audio or other clients. Still, don't do unbounded work per event;
  spawn or batch if you call out to the network.
- `event.kind` is an `EventType`. Its serde name is the wire name
  (`EventType::PositionJump` is `"PositionEvent"` on the wire).
- `EndReason::may_start_next()` tells you whether a `TrackEndEvent` lets a
  client start the next track (Lavalink semantics).

### Full example: an event counter

From `src/counter.rs`:

```rust
use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};
use wavyzon_plugin::serde_json::{self, Value};
use wavyzon_plugin::{EndReason, Event, EventListener, EventType, async_trait};

#[derive(Default)]
pub struct EventCounter {
    by_type: Mutex<BTreeMap<String, u64>>,
    sessions: Mutex<HashSet<String>>,
    total: AtomicU64,
}

impl EventCounter {
    pub fn wire_name(kind: EventType) -> String {
        match serde_json::to_value(kind) { Ok(Value::String(s)) => s, _ => format!("{kind:?}") }
    }
}

#[async_trait]
impl EventListener for EventCounter {
    async fn on_event(&self, session_id: &str, event: &Event) {
        *self.by_type.lock().unwrap().entry(Self::wire_name(event.kind)).or_default() += 1;
        self.sessions.lock().unwrap().insert(session_id.to_string());
        self.total.fetch_add(1, Ordering::Relaxed);

        if event.kind == EventType::TrackExceptionEvent {
            if let Some(f) = &event.exception {
                tracing::warn!(session_id, guild = %event.guild_id, severity = ?f.severity, "track failed: {f}");
            }
        }
        if event.kind == EventType::TrackEndEvent && event.reason == Some(EndReason::LoadFailed) {
            tracing::debug!(guild = %event.guild_id, "load failed");
        }
    }
}
```

Other ideas: scrobbling (`TrackAudibleEvent` + `TrackEndEvent{finished}`),
play history in a database, webhooks, per-guild statistics, lyrics
pre-fetching on `TrackLoadingEvent`.

## 5. HTTP routes

```rust
reg.routes(router);   // mounted at /v1/plugins/<manifest name>
```

- The router is an [axum 0.8](https://docs.rs/axum/0.8) `Router` (use
  `wavyzon_plugin::axum`). Paths are relative: `"/"` is
  `/v1/plugins/<name>/`, `"/stats/{guild}"` is `/v1/plugins/<name>/stats/{guild}`.
- Requests are **authenticated by the node** before they reach you; you never
  see unauthenticated traffic and never need the password.
- Use `.with_state(...)` to give handlers your shared state; the router you
  pass must be `Router<()>` (state already supplied).
- Call `routes` at most once per plugin; merge routers with `Router::merge`
  / `nest` if you build them in pieces.

### Full example: stats routes

From `src/lib.rs`:

```rust
use std::sync::Arc;
use wavyzon_plugin::axum::{Json, Router, extract::State, routing::{get, post}};
use wavyzon_plugin::serde_json::{Value, json};

pub fn routes(shared: Arc<Shared>) -> Router {
    Router::new().route("/", get(stats)).route("/reset", post(reset)).with_state(shared)
}

async fn stats(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({
        "plugin": "example",
        "events": shared.counter.snapshot(),
        "totalEvents": shared.counter.total(),
        "sessions": shared.counter.sessions(),
        "tonesOpened": shared.tones.opened(),
    }))
}

async fn reset(State(shared): State<Arc<Shared>>) -> Json<Value> {
    shared.counter.reset();
    Json(json!({ "reset": true }))
}
```

```bash
curl -H "Authorization: $WAVYZON_PASSWORD" localhost:2333/v1/plugins/example/
curl -X POST -H "Authorization: $WAVYZON_PASSWORD" localhost:2333/v1/plugins/example/reset
```

## 6. Lyrics providers

```rust
#[async_trait]
pub trait LyricsProvider: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn lyrics(&self, track: &Track) -> Result<Option<Lyrics>, Failure>;
}

reg.lyrics(Arc::new(MyLyrics));
```

- `/v1/lyrics` and `/v4/lyrics` ask every provider (yours, LRCLIB,
  lyrics.ovh) in `WAVYZON_LYRICS_ORDER`, matched against `name()` without
  case or punctuation; a provider the variable does not name is asked after
  the named ones. The first `Ok(Some(_))` answers; `Ok(None)` and errors pass
  to the next (errors are logged and counted in `wavyzon_lyrics_plugin_total`).
- `Lyrics::plain(text, provider)` for plain lyrics; fill `lines`
  (`LyricsLine { timestamp_ms, duration_ms, line }`) for timed ones.
  `provider` is shown to clients.
- Registering a provider turns the lyrics routes on (and `lavalyrics-plugin`
  in `/v4/info`) even with the built-in providers off.
- Cache your answers: the node does not. See the `genius` plugin.

## Putting it together

The example plugin's `register` wires all four extension points, sharing one
`Arc<Shared>` between the listener, the source and the routes:

```rust
#[derive(Default, Clone)]
pub struct ExamplePlugin { shared: Arc<Shared> }

#[async_trait]
impl Plugin for ExamplePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest { name: "example", version: env!("CARGO_PKG_VERSION"),
                         description: "Sine-tone source, bitcrush filter, event counter and stats route",
                         api_version: API_VERSION }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let settings = ToneSettings::from_context(ctx)?;      // WAVYZON_PLUGIN_EXAMPLE_*
        reg.source(Arc::new(ToneSource::new(settings, self.shared.clone())))
            .filter(Arc::new(BitcrushFactory))
            .listener(Arc::new(self.clone()))
            .routes(routes(self.shared.clone()));
        Ok(())
    }

    async fn shutdown(&self) {
        tracing::info!(events = self.shared.counter.total(), "example plugin stopped");
    }
}

pub fn plugin() -> Arc<dyn Plugin> { Arc::new(ExamplePlugin::default()) }
```

## Enabling a plugin in the node

In the `wavyzon` binary crate, a plugin is an optional dependency behind a
feature, and is added to the list of compiled-in plugins:

```toml
# crates/wavyzon/Cargo.toml
[dependencies]
wavyzon-plugin-example = { path = "../../plugins/wavyzon-plugin-example", optional = true }

[features]
plugin-example = ["dep:wavyzon-plugin-example"]
```

```rust
// crates/wavyzon/src/plugins.rs
pub fn compiled_in() -> Vec<std::sync::Arc<dyn wavyzon_plugin::Plugin>> {
    let mut v = Vec::new();
    #[cfg(feature = "plugin-example")]
    v.push(wavyzon_plugin_example::plugin());
    v
}
```

```bash
cargo build --release -p wavyzon --features plugin-example
WAVYZON_PLUGINS=example ./target/release/wavyzon
```

For an out-of-tree plugin, add it the same way with a `git` or `path`
dependency in your fork's `crates/wavyzon/Cargo.toml`.

The node checks each selected plugin's `manifest().api_version` against
`API_VERSION`, creates its data directory, calls `registry.begin(name)` and
then `register`, in `WAVYZON_PLUGINS` order (or compiled-in order when it is
empty). Any failure stops the node at startup. On a graceful shutdown
(SIGINT/SIGTERM) every plugin's `shutdown` is awaited after sessions and
voice connections are closed.

## Testing your plugin

Everything a plugin does can be unit-tested without a node. The example
plugin has tests for each part (`cargo test -p wavyzon-plugin-example`):

```rust
#[tokio::test]
async fn registers_every_extension_point() {
    let p = ExamplePlugin::new();
    let ctx = PluginContext { settings: Default::default(), data_dir: "/tmp/x".into(), node_version: "test" };
    let mut reg = Registry::new();
    reg.begin("example");                      // what the node does before register()
    p.register(&ctx, &mut reg).await.unwrap();
    assert_eq!(reg.sources()[0].name(), "tone");
    assert_eq!(reg.filters()[0].name(), "bitcrush");
    assert_eq!(reg.listeners().len(), 1);
    assert_eq!(reg.take_routes()[0].0, "example");
}

#[tokio::test]
async fn streams_exact_length_of_pcm() {
    use tokio::io::AsyncReadExt;
    let src = ToneSource::new(ToneSettings::default(), Default::default());
    let t = src.track("440:1").await.unwrap();
    let mut s = src.playable().unwrap().open(&t, 0).await.unwrap();
    let mut bytes = Vec::new();
    s.body.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes.len(), 48_000 * 4);       // 1 s of s16le stereo
}
```

Test filters by feeding frames of `FRAME_SAMPLES` samples to `process`, and
routes by calling the handler functions directly (or with
`tower::ServiceExt::oneshot`).

## Versioning

`wavyzon_plugin::API_VERSION` is a single integer, currently **`1`**.

- It is bumped when the plugin API changes in a way that breaks existing
  plugins (a trait method added without default, a type changed).
- The node **refuses** a plugin whose `manifest().api_version` differs from
  its own `API_VERSION`, with `PluginError::ApiVersion`, at startup:
  `plugin hello targets API v1, this node speaks v2`.
- Additions that don't break plugins (a new default method, a new
  `EventType` variant) don't bump it. Because events and failures can gain
  variants, match on them with a `_ =>` arm.
- Since plugins are compiled into the node, a mismatch normally shows up as
  a **compile error** first; the runtime check protects against stale
  feature wiring.

| Wavyzon | `API_VERSION` |
| --- | --- |
| 0.1.x | 1 |

## Checklist

- [ ] `name` is lowercase and URL-safe; `api_version: API_VERSION`.
- [ ] Bad settings fail in `register` with `PluginError::Config`.
- [ ] Track identifiers are self-sufficient (survive encode → other node → decode).
- [ ] `Failure` severities: `common` for expected, `suspicious` for refusals, `fault` for bugs.
- [ ] Filters don't allocate, block or lock in `process`; `reset` clears state.
- [ ] Listeners are cheap or offload work.
- [ ] Routes use `wavyzon_plugin::axum` and `with_state`.
- [ ] Unit tests for each part.

## Reference

The API documentation of `wavyzon-plugin` is generated with `cargo doc`:

```bash
cargo doc -p wavyzon-plugin --open
```
