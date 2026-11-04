# Migrating from Cadence

Wavyzon is a rewrite of [Cadence](https://github.com/relateful/Cadence) and
keeps its protocol. Most bots only need configuration changes.

## What stays the same

- The `/v1` REST API and WebSocket, the Lavalink v4 layer and `/version`.
- Every event, its fields and its semantics (`instance`, `TrackAudibleEvent`,
  mix events, `SendStallEvent`…).
- The wire clock anchor and formula; `/v1/clock`.
- Two-deck mixing: `mix/deckb`, `mix/crossfade`, `mix/host`, profiles,
  `nextTrack`, analysis fingerprints.
- Filters, including `reverb`, and their glides.
- Encoded tracks are base64url JSON of the same model, so tracks encoded by
  Cadence decode on Wavyzon, as long as their `source` exists on Wavyzon
  (see Deezer below).

## What changes

### Configuration prefix

Every `CADENCE_*` variable is now `WAVYZON_*`:

```bash
sed -i 's/^CADENCE_/WAVYZON_/' .env
```

| Cadence | Wavyzon |
| --- | --- |
| `CADENCE_PASSWORD` | `WAVYZON_PASSWORD` |
| `CADENCE_ADDRESS` | `WAVYZON_ADDRESS` |
| `CADENCE_BUFFER_MS`, `_BITRATE`, `_PLAYOUT_DELAY_MS`, `_PAUSE_FADE_MS`, `_RESUME_TIMEOUT_SECONDS`, `_UPDATE_INTERVAL_MS`, `_SEARCH_ORDER`, `_ANALYSIS_WORKERS`, `_ANALYSIS_QUEUE`, `_LOG_LEVEL`, `_AUDIO_CACHE_DIR`, `_AUDIO_CACHE_GB`, `_NORMALIZE`, `_NORMALIZE_LUFS`, `_STATE_FILE`, `_CACHE_PEERS`, `_CACHE_SECRET` | same names with `WAVYZON_` |
| `CADENCE_ALLOW_NO_VOICE` | `WAVYZON_ALLOW_NO_VOICE` (every regular build has voice; this only matters for a build without the `opus`/`dave` features) |
| `SPOTIFY_*`, `APPLE_MUSIC_*`, `SOUNDCLOUD_*`, `*_PROXY` | unchanged |

> Note: provisional. Whether the source credentials (`SPOTIFY_*`,
> `APPLE_MUSIC_*`, `SOUNDCLOUD_*`) also get a `WAVYZON_` prefix.

### Deezer is metadata only

Cadence played Deezer streams (with `DEEZER_ARL` and `DEEZER_MASTER_KEY`) and
used SoundCloud as a fallback. Wavyzon does **not** decrypt Deezer: Deezer is
a metadata source like Spotify and Apple Music, and its tracks are mirrored
to `WAVYZON_PLAYBACK_SOURCES`.

- Remove `DEEZER_ARL`, `DEEZER_ARL_ROTATION`, `DEEZER_MASTER_KEY`,
  `DEEZER_HIGH_QUALITY`.
- Set `WAVYZON_PLAYBACK_SOURCES` (e.g. `soundcloud`).
- Stored Deezer tracks still decode and play: they are mirrored on play.
- Metrics that were Deezer-specific (`cadence_deezer_accounts_total`,
  `cadence_cdn_range_ignored_total`) and `cadence_seek_total` are gone;
  `wavyzon_fallback_total` now counts playback-source fallbacks, and
  `wavyzon_mirror_total` is new. See [Observability](../observability.md).

### New things

- HTTP source (direct files and radio) and local files.
- Plugins: `GET /v1/plugins`, `/v1/plugins/{name}/…`, `pluginFilters`,
  plugin sources in `/v1/info`.
- v4 extra events switch: `wavyzonEvents` (`cadenceEvents` is accepted).
- `WebSocketClosedEvent` on `/v1` names the reason text `closeReason` and
  has no `byRemote`; on `/v4` it keeps Lavalink's `reason` and `byRemote`.
- `/v4/info` reports `wavyzon` next to Cadence's `cadence` field, and lists
  plugins.
- A player update may use Lavalink's `track.identifier`.
- `WAVYZON_PLAYBACK_SOURCES` replaces Cadence's Deezer-first playback with
  SoundCloud fallback.
- `WAVYZON_LOG_FORMAT` (`json`/`pretty`) and `RUST_LOG` are new.
- `/v1/cache/index` keys are Wavyzon's (`<sha256 prefix>-<format>`), so a
  Cadence node and a Wavyzon node can't share a peer cache.

### Build and deploy

| Cadence | Wavyzon |
| --- | --- |
| Go + cgo, `libopus`, `libdave` (`libdave_install.sh`), `-tags native` | `cargo build --release -p wavyzon`; Opus static, DAVE in Rust |
| `cadence` binary | `wavyzon` binary |
| `docker build -t cadence .` | `docker build -t wavyzon .` |

### Metrics

`cadence_*` metrics are now `wavyzon_*`. Update dashboards and alerts.
A few series also changed shape:

- `host_total` counts `started`/`ended`/`refused` (Cadence:
  `played`/`dropped`/`refused`).
- `recommendations_total` is labelled by `outcome`
  (`served`/`cached`/`empty`/`failed`), not by `source`.
- The Go runtime gauges (`goroutines`, `heap_bytes`,
  `gc_pause_total_seconds`) are replaced by `tasks` and `resident_bytes`.

### Smaller behaviour changes

- The default `WAVYZON_SEARCH_ORDER` ends with `soundcloud`
  (`applemusic,spotify,deezer,soundcloud`).
- Oversized request bodies return `413` instead of `400`.
- `livecheck` checks only the first query unless `-bench` is given, and
  `-rec` takes a search query instead of a Deezer seed id.

### State file

Wavyzon's state file has the same layout as Cadence's (version 1, and
`cadenceEvents` is read), but saved voice state and filters may not load
from a file written by Cadence. Drain or let sessions end before switching,
and start Wavyzon with a fresh state file.

### License

Cadence is MIT; Wavyzon is **CC BY-NC-SA 4.0** (non-commercial, share-alike).
Check that this fits your use before migrating.

## Checklist

- [ ] Rename `CADENCE_*` → `WAVYZON_*`.
- [ ] Remove Deezer streaming variables; set `WAVYZON_PLAYBACK_SOURCES`.
- [ ] Rebuild or pull the Wavyzon image.
- [ ] If you use v4 extras, switch to `wavyzonEvents` (optional).
- [ ] Update dashboards to `wavyzon_*` metrics.
- [ ] Start with a fresh state file.
