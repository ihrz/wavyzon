# Migrating from Lavalink

You can migrate in two steps: first **swap the node** and keep your client
on the Lavalink v4 protocol, then, if you want the extras, **adopt `/v1`**.

## Step 1: swap the node

### 1. Map your configuration

Lavalink's `application.yml` becomes environment variables:

| Lavalink (`application.yml`) | Wavyzon |
| --- | --- |
| `server.port`, `server.address` | `WAVYZON_ADDRESS=0.0.0.0:2333` |
| `lavalink.server.password` | `WAVYZON_PASSWORD` (not `youshallnotpass`) |
| `lavalink.server.sources.soundcloud` | `SOUNDCLOUD_ENABLED` |
| `lavalink.server.sources.http` | `WAVYZON_HTTP_SOURCE` |
| `lavalink.server.sources.local` | `WAVYZON_LOCAL_ROOT=/path` |
| `lavalink.server.sources.youtube` / youtube-source plugin | not available |
| `lavalink.server.bufferDurationMs` | `WAVYZON_BUFFER_MS` |
| `lavalink.server.playerUpdateInterval` (s) | `WAVYZON_UPDATE_INTERVAL_MS` (ms) |
| `lavalink.server.opusEncodingQuality` | `WAVYZON_BITRATE` (and per-player `bitrate`) |
| `lavalink.server.filters.*` | always enabled |
| `lavalink.server.ratelimit` (route planner) | not available; use `*_PROXY` per source |
| LavaSrc `spotify.clientId/clientSecret` | `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET` |
| LavaSrc `applemusic.mediaAPIToken`, `countryCode` | `APPLE_MUSIC_TOKEN`, `APPLE_MUSIC_STOREFRONT` |
| LavaSrc `deezer.*` | Deezer metadata needs nothing; Wavyzon does not stream Deezer |
| LavaSrc `providers` (mirroring queries) | `WAVYZON_PLAYBACK_SOURCES` (ISRC first, then scored search) |
| `logging.level.*` | `WAVYZON_LOG_LEVEL` / `RUST_LOG` |
| `metrics.prometheus.enabled` | always on at `/metrics` (authenticated) |

### 2. Point your client at Wavyzon

Change host, port and password. Nothing else is required. See
[client setup](../lavalink.md#client-setup) for lavalink-client, Shoukaku,
Wavelink and others.

### 3. Fix searches

Replace `ytsearch:` / `ytmsearch:` with `scsearch:` (playback) or `spsearch:`
/ `amsearch:` / `dzsearch:` (metadata, then mirrored), and change your
client's default search platform. Direct URLs (files, radio) work as-is.

### 4. Re-resolve stored tracks

If you stored Lavalink-encoded tracks (saved playlists, queues in a
database), they won't decode on Wavyzon. Store the track **URI** (or
`sourceName` + `identifier`) and resolve again with `loadtracks`. Going
forward, Wavyzon-encoded tracks are portable across Wavyzon nodes and
versions.

### 5. Test

- Play, pause, seek, volume, filters, stop, skip.
- Disconnect the bot and reconnect (resume).
- Move the bot between voice channels; check `WebSocketClosedEvent` handling.

## Step 2: adopt the extras

On `/v4`, enable the extra events (`{"wavyzonEvents": true}`) to get
`TrackAudibleEvent` and mix events without changing protocol, or move to
`/v1`:

| Lavalink v4 | Wavyzon `/v1` |
| --- | --- |
| `/v4/websocket` | `/v1/websocket` |
| `/v4/loadtracks`, `/v4/decodetrack(s)` | `/v1/…`, track `info` uses `source`, `durationMs`, … instead of `sourceName`, `length` |
| `/v4/sessions/{id}/players/{guild}` | `/v1/…` same body; the player state adds the wire-clock anchor and `playable` |
| `/v4/info`, `/v4/stats` | `/v1/info`, `/v1/stats` (different shapes, see [REST](../api/rest.md)) |
| `playerUpdate.state.position` | `playerUpdate.state.player.positionMs` + the anchor |
| Standard events only | All events, see [WebSocket & events](../api/websocket.md) |

Then, depending on what your bot does:

- **Lyrics / now-playing bars**: use the [wire clock](../wire-clock.md)
  formula and `TrackAudibleEvent`.
- **Gapless queues**: send `nextTrack` when a track starts.
- **Automix**: see [Mixing](../mixing.md).
- **Custom sources or effects**: [write a plugin](../plugins.md) instead of a
  Lavalink plugin.
