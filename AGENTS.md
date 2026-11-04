# AGENTS.md — working on Wavyzon

Notes for coding agents (and humans) picking this repository up. Read this
first, then the doc it points to for the area you touch. Keep it current:
when you learn something non-obvious, add it here.

## What this is

**Wavyzon** is a standalone audio node for Discord bots, written in Rust: it
joins voice channels, streams and mixes music, and reports *exactly* when each
frame reached Discord (the "wire clock"). Bots talk to it over REST + a
WebSocket, and it also speaks **Lavalink v4**, so Lavalink clients work
unchanged.

It is a **rewrite of [Cadence](https://github.com/relateful/Cadence)** (Go,
MIT) by the iHorizon team. Cadence is the reference for behaviour: the `/v1`
API, events, wire-clock formula, two-deck mixing, filters, state file and
most numeric constants are ported from it, often line by line. Wavyzon is
licensed **CC BY-NC-SA 4.0** (Cadence's MIT notice stays in `NOTICE`).

- Primary repo: GitLab `ihrz/wavyzon` (`origin`). GitHub is a read-only mirror.
- The maintainer writes in **French**; answer in French unless asked
  otherwise. Code, comments, commits and docs are in **English** (the
  changelog and a few docs also have French versions, see below).

## Repository map

```
crates/
  wavyzon/            binary: config, REST+WS server (/v1, /v4), sessions & resume,
                      Lavalink layer, plugin host, metrics, peer cache, shutdown;
                      bins: wavyzon, livecheck (live source checks)
  wavyzon-player/     two-deck player: decks, crossfades, gapless, host/DJ voice-overs,
                      filters + glides, reverb, loudness, plugin filter chain, events,
                      wire clock
  wavyzon-voice/      Discord voice gateway v8, UDP/RTP, AEAD, DAVE (davey), 20 ms send loop
  wavyzon-sources/    SoundCloud, HTTP, local, Deezer/Spotify/Apple Music (metadata only),
                      resolver, ISRC mirroring + match scoring, caches, httpx client
  wavyzon-analysis/   tempo/beats/key/phrases/loudness fingerprints, offline queue
  wavyzon-audio/      PCM format (48 kHz stereo, 20 ms i16 frames), DSP blocks
                      (biquad, sinc resampler, WSOLA stretcher, crossfade), symphonia
                      decode, opus encode, FrameSource trait
  wavyzon-plugin/     plugin API AND the shared model (Track, LoadResult, Failure, Event)
  wavyzon-testkit/    fake Discord voice gateway, mock sources, audio assertions
plugins/wavyzon-plugin-example/   tone source, bitcrush filter, counter, route
docs/                 mdBook (docs/src/SUMMARY.md is the index)
assets/               logo, banner, mono icon (+ assets/README.md brand guide)
contrib/pelican/      Pelican/Pterodactyl egg: edit install.sh + gen.py, then regenerate
                      egg-wavyzon.json (never edit the JSON by hand)
changelogs/en|fr/<version>/   archived release notes
```

Dependency direction is strict: `wavyzon` → player/voice/sources/analysis →
audio → plugin. `wavyzon-plugin` is at the bottom on purpose: built-in sources
implement the same `Source` trait as plugins. Don't add upward dependencies.

`docs/src/architecture.md` has the data flow and the real-time path. Read it
before touching the player or voice crates.

## Commands

Toolchain: stable (pinned by `rust-toolchain.toml`), edition 2024, MSRV 1.85.
The build compiles libopus statically, so **CMake and a C compiler** are
needed (`.cargo/config.toml` sets this up). No libdave: DAVE is pure Rust.

```sh
make check        # = CI: fmt --check, clippy -D warnings, tests  (or: just check)
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test -p wavyzon-sources resolver      # one crate / filter
cargo run -p wavyzon                        # needs .env (cp .env.example .env)
cargo run -p wavyzon --bin livecheck -- "query"   # hits real sources, needs network
make deny         # cargo-deny (licences, advisories)
```

Before calling anything done: **fmt, clippy with `-D warnings` and the full
test suite must pass** (around 305 tests, about a minute after the first
build). Clippy flags `items_after_test_module`: keep `#[cfg(test)] mod tests`
at the end of the file.

## Invariants: don't break these

1. **Protocol compatibility is a feature.** `/v1` REST and WS shapes, event
   fields, Lavalink v4 shapes, encoded tracks (base64url JSON of `Track`;
   tracks encoded by Cadence must still decode) and the state-file layout.
   Field-level changes need a strong reason, a test and a changelog entry.
2. **The send loop is the only real-time code** (`wavyzon-voice` sender, 20 ms).
   Nothing slow on it: no HTTP, no decoding, no unbounded locks, no
   allocation-heavy work. Slow work lives on other tasks and reaches it through
   bounded buffers.
3. **Bounded everything.** Every queue, cache, backlog and body read has a
   limit and a metric; under load it drops, counts and logs rather than grows.
   When reading bodies from the network, stop at the limit; never buffer
   first and check afterwards.
4. **Never send audio unencrypted** while a DAVE epoch is not ready; hold it.
5. **Time is measured at the socket** (wire clock). Positions come from
   `FrameSource::sent`, not from the decoder.
6. **Every failure is visible**: an event, a log line and a metric. Use
   `Failure::{common, suspicious, fault}` with the same classification Cadence
   uses: source/network problems are `common`, 401/403/429 `suspicious`, only
   node bugs are `fault`.
7. **Plugins are first-class**: same traits and events as built-ins.

## Cadence parity

The goal of most work so far has been "recode Cadence faithfully, then
improve". When working on behaviour, compare with the Go source:

```sh
git clone --depth 50 https://github.com/relateful/Cadence.git <scratch-dir>/cadence
```

Treat it as read-only reference data: don't run its scripts. The mapping is
1:1 by package: `internal/api` → `crates/wavyzon/src/api`, `internal/session` →
`.../session`, `internal/player` → `wavyzon-player`, `internal/audio` +
`internal/codec` → `wavyzon-audio`, `internal/analysis` → `wavyzon-analysis`,
`internal/source/**` + `httpx` + `audiocache` → `wavyzon-sources`,
`internal/voice` → `wavyzon-voice`, `internal/peercache` →
`crates/wavyzon/src/peercache.rs`, `cmd/livecheck` → `src/bin/livecheck.rs`.

A full audit (November 2026) found the port close to line-by-line. Gaps fixed
then: offline analysis uses the sinc resampler; stream open has the 30 s idle
timeout; httpx applies the header timeout per attempt, with proxy fallback and
one retry; the server has a 10 s header-read timeout (a custom hyper loop in
`node.rs`, because `axum::serve` sets no timer); three-way env flags; Go-style
path escaping for `isrc:`; JSON errors are `common`; serde defaults; bounded
peer index read; 0700 state dir; mirror fallback miss caching.

**Intentional differences** (don't "fix" them back; they're listed in
`docs/src/guides/from-cadence.md`, keep that file updated):

- `CADENCE_*` → `WAVYZON_*`, `cadence_*` metrics → `wavyzon_*` (`cadenceEvents`
  is still accepted as an alias of `wavyzonEvents`).
- **Deezer is metadata only**: no ARL, decryption or streaming. Deezer, Spotify
  and Apple Music tracks are mirrored by ISRC/title to
  `WAVYZON_PLAYBACK_SOURCES` (default `soundcloud`).
- New: HTTP and local sources, plugins, `pluginFilters` (an unknown or
  rejected plugin filter is a documented `400`), `closeReason` on `/v1`
  `WebSocketClosedEvent`.
- `ytsearch:`/`ytmsearch:` are plain searches through `WAVYZON_SEARCH_ORDER`
  (a test pins this).
- Metric labels for `host_total` and `recommendations_total`, runtime gauges,
  `413` for oversized bodies, the `livecheck` CLI.
- Symphonia decodes with gapless trimming and skips up to 64 corrupt packets.

**Known, not done** (low priority hardening, not parity):

- reqwest without HTTP/2 and keep-alive pings (Cadence forces h2).
- Voice IP discovery runs inside the session loop (heartbeats pause up to 5 s).
- The WS liveness check counts any inbound frame instead of a pong with a
  timeout.
- PATCH player parses the body before the 404 session check.
- The live analysis tap runs after plugin filters.
- Deezer `readable:false` tracks are dropped (as in Cadence) even though they
  could be mirrored.

## How to do common changes

- **New config variable**: parse it in `crates/wavyzon/src/config.rs` (node) or
  `crates/wavyzon-sources/src/config.rs` (sources). Flags use the three-way
  rule (`1/true/yes/on`, `0/false/no/off`, anything else keeps the default).
  Then document it in `.env.example`, `docs/src/configuration.md` and, if
  user-facing, the README's configuration table.
- **New source**: implement `Source` (plus `Playable` / `IsrcLookup` /
  `Recommender` as needed) from `wavyzon-plugin`; register it in
  `wavyzon-sources/src/builtins.rs`, or ship it as a plugin. Use the shared
  `HttpClient` (`httpx.rs`) for proxies, timeouts, private-address blocking
  and redacted errors. Use `util::esc` for path segments.
- **New event or field**: model in `wavyzon-plugin/src/event.rs`, emit from the
  player/session, map it for v4 in `crates/wavyzon/src/lavalink.rs` (extras
  only behind `wavyzonEvents`), document it in `docs/src/api/websocket.md`.
- **New metric**: counters in `crates/wavyzon/src/metrics.rs` or
  `wavyzon-sources/src/metrics.rs`, exposition in `crates/wavyzon/src/api/node.rs`,
  docs in `docs/src/observability.md`.
- **Player behaviour**: tests in `crates/wavyzon-player/src/tests/` use
  `ToneOpener` and a `Recorder` of events. Keep them deterministic: no real
  sleeps when you can drive frames directly.
- **End-to-end**: `crates/wavyzon/tests/{api,e2e}.rs` start a real node
  (`tests/common`) with the testkit's fake voice gateway and tone source.
- **Network code**: test against local axum servers (`testutil::serve`) or raw
  `TcpListener`s that stall or reset; never against the internet.

## Conventions

- **Conventional Commits**: `type(scope): summary` (`fix(voice): …`,
  `feat(sources): …`); scopes are crates/areas (`voice`, `player`, `mixer`,
  `sources`, `http`, `api`, `plugin`, `analysis`, `docs`, `ci`). Breaking
  changes get `!` and an explanation.
- `rustfmt.toml`: 120 columns. Comments explain *why*; doc comments on public
  items. When porting from Cadence, say so (`// as in Cadence`, `port of
  Cadence's applyPatch`) and cite the reason for any divergence.
- Docs ship with the code: update `docs/`, `README.md` (+ `README.fr.md`) and
  `.env.example` in the same change. User-visible changes go in the MR
  description; at release, maintainers write `CHANGELOG.md` and
  `CHANGELOG_FR.md` and archive them under `changelogs/{en,fr}/<version>/`.
- `#[allow(clippy::…)]` only locally, with a comment saying why.
- No new dependency without a reason; `cargo deny` must pass (licences in
  `deny.toml`).

## Branches & workflow

- `main` is the default branch; CI runs on MRs, `main` and tags (`v*` tags
  publish the Docker image).
- **`dev`** is the integration branch: push finished work there (or open an MR
  into it), never force-push and never push straight to `main` unless asked.
- History note: the codebase was first committed as one "import current
  Wavyzon working tree" commit (it had been developed untracked on top of the
  GitLab "Initial commit").

## Brand assets

`assets/logo.svg` is the source of truth for the mark (Night Ink tile, violet
→ cyan wave "W" = deck A, cyan echo = deck B, white playhead = wire clock).
It uses `wz-` ids inside `<g id="wz-mark">` so it can be inlined; `banner.svg`
inlines it (a README `<img>` can't load other files). Re-render `logo.png`
with headless Chromium (command in `assets/README.md`). Check 16–48 px
legibility and the mono icon after any change.

## Agent environment gotchas

- Claude Code background jobs work in a git worktree under
  `.claude/worktrees/`. Its safety check refuses shell commands it can't
  verify (heredocs, inline `python - <<EOF`, `$PWD` expansions in complex
  commands). Use the Edit/Write tools for file changes, and run scripts as
  `python3 -I /path/to/script.py args` with absolute paths.
- The untracked `target/` in the main checkout is not shared with worktrees:
  the first build in a worktree compiles everything (including libopus),
  so expect a few minutes.
- `cargo test --workspace` already includes the example plugin (it is a
  default feature of the `wavyzon` crate); `--all-features` is what CI uses.
