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
| `SPOTIFY_*`, `APPLE_MUSIC_*`, `SOUNDCLOUD_*`, `DEEZER_*`, `*_PROXY` | unchanged |

> Note: provisional. Whether the source credentials (`SPOTIFY_*`,
> `APPLE_MUSIC_*`, `SOUNDCLOUD_*`) also get a `WAVYZON_` prefix.

### Deezer streaming

Cadence played Deezer streams (with `DEEZER_ARL` and `DEEZER_MASTER_KEY`) and
used SoundCloud as a fallback. Wavyzon ports that: `DEEZER_ARL`,
`DEEZER_ARL_ROTATION`, `DEEZER_MASTER_KEY` and `DEEZER_HIGH_QUALITY` work
unchanged (same gw login, same `BF_CBC_STRIPE` decryption, same account
rotation and benching, same range seeks).

- The defaults changed: `WAVYZON_PLAYBACK_SOURCES` is `deezer,soundcloud` and
  `WAVYZON_SEARCH_ORDER` starts with `deezer`, so everything plays through
  Deezer when it can.
- Without `DEEZER_ARL`/`DEEZER_MASTER_KEY`, Deezer stays metadata only and
  its tracks are mirrored to `WAVYZON_PLAYBACK_SOURCES` (with a startup
  warning), which Cadence could not do.
- The Deezer metrics are renamed: `cadence_deezer_accounts_total` →
  `wavyzon_deezer_accounts_total`, `cadence_cdn_range_ignored_total` →
  `wavyzon_cdn_range_ignored_total`, `cadence_seek_total` →
  `wavyzon_seek_total` (outcomes `range`/`fallback`; seeks into a
  disk-cached track are served from disk). `wavyzon_fallback_total` counts
  playback-source fallbacks and `wavyzon_mirror_total` is new. See
  [Observability](../observability.md).

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
- Lyrics from lyrics.ovh on `/v1/lyrics` and `/v4/lyrics` (LavaLyrics shape,
  `LYRICS_ENABLED`, `LYRICS_API_URL`, `LYRICS_PROXY`, `lyrics_total`).
- `WAVYZON_CROSSFADE_MS` (default `3000`): the `nextTrack` crossfades in
  before the current track ends instead of Cadence's strict gapless swap.
  Set it to `0` for Cadence's behaviour.
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
- `ytsearch:`/`ytmsearch:` follow `WAVYZON_SEARCH_ORDER` like plain text;
  Cadence sent them straight to its audio source (Deezer).
- Mirroring is stricter: a candidate's title must match in both directions
  (an upload called "LUTHER - NOTORIOUS UET" no longer stands in for
  Luther's "uet"), a remix/live/sped-up version is never played for the
  original, and ties go to the better-ranked search result. When no real
  match exists the track fails with "no matching recording" instead of
  playing the wrong song.
- Oversized request bodies return `413` instead of `400`.
- A stream that breaks mid-track is reopened where the decoder stopped (up
  to three times, not for live streams) instead of failing with `stream
  interrupted`: a preloaded `nextTrack` or a long pause no longer loses
  its connection for good.
- A player update with `track` and `position` never seeks, even when
  `noReplace` keeps the current track (Lavalink's rule). Cadence seeked, so
  lavalink-client's queue advance (`noReplace` + `position: 0`) rewound a
  track that a gapless swap or crossfade had just started. Such a
  `noReplace` play of that track also adopts it at once.
- SoundCloud also plays HLS AAC (fragmented MP4), which is all many tracks
  offer now. Cadence's `audio/mpeg` prefix test also matched the adaptive
  `audio/mpegurl` playlist, which SoundCloud answers with a 404, so those
  tracks failed with "not found at source".
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
- [ ] Keep `DEEZER_ARL` + `DEEZER_MASTER_KEY` (same variables and meaning).
- [ ] Rebuild or pull the Wavyzon image.
- [ ] If you use v4 extras, switch to `wavyzonEvents` (optional).
- [ ] Update dashboards to `wavyzon_*` metrics.
- [ ] Start with a fresh state file.
