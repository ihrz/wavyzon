# REST API

Wavyzon's native API lives under **`/v1`**. Its shape follows Lavalink v4
closely, so if you know Lavalink you already know most of it. The Lavalink v4
routes themselves (`/v4/…`) are documented in
[Lavalink v4 compatibility](../lavalink.md).

## Conventions

**Authentication.** Every route, including the WebSocket upgrade and
`/metrics`, requires the node password:

```http
Authorization: <WAVYZON_PASSWORD>
```

The value is the bare password (no `Bearer` prefix). A missing or wrong
header returns `401`.

**Bodies** are JSON, at most 1 MiB. Responses are JSON with
`Content-Type: application/json` unless noted.

**Errors** always have this shape:

```json
{
  "timestamp": 1767225600000,
  "status": 404,
  "error": "Not Found",
  "message": "unknown session",
  "path": "/v1/sessions/abc/players/123"
}
```

| Status | When |
| --- | --- |
| `400` | Malformed JSON, invalid encoded track, invalid filter values, missing parameter |
| `401` | Missing or wrong `Authorization` |
| `404` | Unknown session, no player for this guild, unknown fingerprint |
| `409` | The action conflicts with the player's state (crossfade with no deck B, seek with nothing playing…) |
| `422` | The track can't be played (deck B load failed, or it could not be resolved) |
| `500` | A bug. The handler is isolated; the node keeps running |

**Tracks.** A track travels as an *encoded* string: unpadded base64url of
the track's JSON. Any Wavyzon node can decode and play a track resolved by any
other node. In responses a track is always paired with its decoded `info`:

```json
{
  "encoded": "eyJzb3VyY2UiOiJzb3VuZGNsb3VkIiwiaWRlbnRpZmllciI6IjEyMzQ1Njc4OTAiLCJ0aXRsZSI6Ik1pZG5pZ2h0IENpdHkiLCJhdXRob3IiOiJNODMiLCJkdXJhdGlvbk1zIjoyNDMwMDAsImlzcmMiOiJGUjZWODExNDEwNjEiLCJhcnR3b3JrVXJsIjoiaHR0cHM6Ly9pMS5zbmRjZG4uY29tL2FydHdvcmtzLTAwMC10NTAweDUwMC5qcGciLCJ1cmkiOiJodHRwczovL3NvdW5kY2xvdWQuY29tL204My9taWRuaWdodC1jaXR5In0",
  "info": {
    "source": "soundcloud",
    "identifier": "1234567890",
    "title": "Midnight City",
    "author": "M83",
    "durationMs": 243000,
    "isrc": "FR6V81141061",
    "artworkUrl": "https://i1.sndcdn.com/artworks-000-t500x500.jpg",
    "uri": "https://soundcloud.com/m83/midnight-city"
  }
}
```

| `info` field | Type | Notes |
| --- | --- | --- |
| `source` | string | Owning source: `soundcloud`, `http`, `local`, `deezer`, `spotify`, `applemusic`, or a plugin source |
| `identifier` | string | Source-specific id (a URL for `http`, a relative path for `local`) |
| `title`, `author` | string | Always present, may be empty |
| `album` | string | Omitted when unknown |
| `durationMs` | integer | `0` when unknown (live streams) |
| `isrc` | string | Omitted when unknown. Used for [mirroring](../sources.md#mirroring) |
| `artworkUrl`, `uri` | string | Omitted when unknown |
| `isStream` | boolean | `true` for endless streams (radio). Omitted when `false` |

Treat `encoded` as opaque: don't build it yourself unless you have to, and
never assume two encodings of the same track are byte-identical.

---

## Node

### `GET /v1/info`

What the node can do.

```json
{
  "version": "0.1.0",
  "sources": ["deezer", "spotify", "applemusic", "soundcloud", "http", "local", "tone"],
  "dave": true,
  "filters": ["equalizer", "timescale", "tremolo", "vibrato", "rotation", "lowPass", "karaoke", "reverb"],
  "pluginFilters": ["bitcrush"],
  "plugins": [
    { "name": "example", "version": "0.1.0", "description": "Sine-tone source, bitcrush filter, event counter and stats route", "apiVersion": 1 }
  ],
  "mixing": true,
  "analysis": 3,
  "wireClock": true,
  "lyrics": true
}
```

| Field | Meaning |
| --- | --- |
| `sources` | Enabled sources, built-in and plugin |
| `dave` | DAVE end-to-end encryption is available (always `true` in Wavyzon) |
| `filters` | Built-in filter keys accepted in `filters` |
| `pluginFilters` | Keys accepted under `filters.pluginFilters` |
| `plugins` | Loaded plugins (same as [`GET /v1/plugins`](#get-v1plugins)) |
| `analysis` | Version of the track analysis format |
| `wireClock` | Positions are measured at the socket (always `true`) |
| `lyrics` | [Lyrics](#lyrics) are served (`LYRICS_ENABLED`) |

`pluginFilters` and `plugins` are new in Wavyzon.

### `GET /v1/stats`

```json
{
  "players": 12,
  "playingPlayers": 9,
  "sessions": 2,
  "uptime": 86400000,
  "analysis": { "queued": 1, "dropped": 0, "done": 412, "failed": 3 }
}
```

`uptime` is in milliseconds. A player is *playing* when it is sending audio
(`sounding`).

### `GET /v1/clock`

The node's wall clock in Unix milliseconds, for [clock sync](../wire-clock.md#clock-sync).

```json
{ "now": 1767225600123 }
```

### `GET /metrics`

Prometheus text exposition (`text/plain; version=0.0.4`). Requires
`Authorization` like everything else. See [Observability](../observability.md).

### `GET /v1/plugins`

Loaded plugins, in registration order.

```json
[
  { "name": "example", "version": "0.1.0", "description": "Sine-tone source, bitcrush filter, event counter and stats route", "apiVersion": 1 }
]
```

### `/v1/plugins/{name}/…`

Routes contributed by plugin `{name}`, mounted at `/v1/plugins/{name}` (a
trailing slash is ignored, so `/v1/plugins/example/` and
`/v1/plugins/example` are the same route). They are authenticated by the node
before the plugin sees the request, so plugins never handle the password.
Unknown plugin names return `404`.
The example plugin serves:

```http
GET /v1/plugins/example/
```

```json
{
  "plugin": "example",
  "version": "0.1.0",
  "events": { "PositionEvent": 1, "TrackAudibleEvent": 4, "TrackEndEvent": 3, "TrackStartEvent": 4 },
  "totalEvents": 12,
  "sessions": 1,
  "tonesOpened": 2
}
```

---

## Tracks

### `GET /v1/loadtracks?identifier=…`

Resolve a link, a prefixed search, an ISRC or plain text. URL-encode the
identifier.

| Identifier | Resolved by |
| --- | --- |
| `https://soundcloud.com/…`, `https://open.spotify.com/…`, `https://music.apple.com/…`, `https://www.deezer.com/…`, short links | The source that owns the link |
| Any other `http(s)://` URL | The [HTTP source](../sources.md#http) (direct files, radio, playlists `.m3u`/`.pls`) |
| `scsearch:…`, `spsearch:…`, `amsearch:…`, `dzsearch:…` | Search on that source |
| `dzisrc:USUM71703861` | Deezer lookup by ISRC |
| `local:path/in/root.flac` | The [local source](../sources.md#local-files) |
| `<prefix>:…` registered by a plugin | That plugin's source (`tone:440`) |
| Plain text | Searched on `WAVYZON_SEARCH_ORDER` in order |

> Note: provisional. The `local:` prefix and `.m3u`/`.pls` playlist support
> in the HTTP source.

The response has a `loadType` and, depending on it, `data`:

**`track`**: a single track.

```json
{ "loadType": "track", "data": { "encoded": "eyJzb3…", "info": { "source": "http", "identifier": "https://example.com/song.mp3", "title": "song.mp3", "author": "example.com", "durationMs": 184000, "uri": "https://example.com/song.mp3" } } }
```

**`playlist`**: an album, set or playlist.

```json
{
  "loadType": "playlist",
  "data": {
    "info": { "name": "Hurry Up, We're Dreaming", "artworkUrl": "https://…/cover.jpg", "uri": "https://www.deezer.com/album/1234", "total": 22 },
    "tracks": [ { "encoded": "eyJzb3…", "info": { "source": "deezer", "identifier": "3135556", "title": "Midnight City", "author": "M83", "durationMs": 243000, "isrc": "FR6V81141061" } } ]
  }
}
```

`total` is the size of the playlist at the source; `tracks` may be shorter
(Spotify playlists load at most 1 000 tracks).

**`search`**: search results, best first.

```json
{ "loadType": "search", "data": [ { "encoded": "eyJzb3…", "info": { … } }, { "encoded": "eyJzb3…", "info": { … } } ] }
```

**`empty`**: nothing found. No `data`.

```json
{ "loadType": "empty" }
```

**`error`**: the source failed.

```json
{ "loadType": "error", "data": { "severity": "suspicious", "message": "refused by source", "cause": "HTTP 429", "source": "soundcloud" } }
```

| `severity` | Meaning |
| --- | --- |
| `common` | Expected in normal operation: not found, timed out, region locked |
| `suspicious` | The source refused us (401, 403, 429): possibly rate-limited or blocked |
| `fault` | A bug or an unrecoverable node-side problem |

`loadtracks` has a 20 s deadline. As a side effect it **prefetches** its
first result so the common "search, then play" flow starts from memory.

### `GET /v1/decodetrack?encodedTrack=…`

```json
{ "encoded": "eyJzb3…", "info": { "source": "soundcloud", "identifier": "1234567890", "title": "Midnight City", "author": "M83", "durationMs": 243000 } }
```

### `POST /v1/decodetracks`

Body: an array of encoded tracks. Response: an array of `{encoded, info}` in
the same order. One invalid track fails the whole request with `400`.

```json
["eyJzb3…", "eyJzb3…"]
```

### `POST /v1/prefetch`

Preload the start of up to 10 upcoming tracks, so they start instantly when
played (see [Sources › Instant starts](../sources.md#instant-starts)).

```json
{ "tracks": ["eyJzb3…", "eyJzb3…"] }
```

`202 Accepted`:

```json
{ "queued": 2, "dropped": 0 }
```

`dropped` counts tracks the bounded prefetch queue had no room for.

---

## Sessions

A session is created when a client opens the [WebSocket](websocket.md). Its
id comes in the `ready` message.

### `PATCH /v1/sessions/{session}`

Configure resuming.

```json
{ "resuming": true, "timeout": 300 }
```

| Field | Type | Meaning |
| --- | --- | --- |
| `resuming` | boolean | Keep players running when the WebSocket drops, and replay missed messages on reconnect |
| `timeout` | integer, seconds | How long to wait for the client before destroying the session (default `WAVYZON_RESUME_TIMEOUT_SECONDS`) |
| `wavyzonEvents` | boolean | v4 sessions only: also send Wavyzon's extra events. See [Lavalink compatibility](../lavalink.md#extra-events). Cadence's name `cadenceEvents` is accepted too |

Response: the effective settings.

```json
{ "resuming": true, "timeout": 300 }
```

---

## Players

A player exists per guild per session. It is created by the first `PATCH`.

### `GET /v1/sessions/{session}/players`

An array of player states (the same object as a [`playerUpdate`](websocket.md#playerupdate)).

### `GET /v1/sessions/{session}/players/{guild}`

```json
{
  "op": "playerUpdate",
  "guildId": "1046781946263277578",
  "state": {
    "time": 1767225600123,
    "connected": true,
    "ping": 21,
    "region": "rotterdam",
    "player": {
      "track": { "source": "soundcloud", "identifier": "1234567890", "title": "Midnight City", "author": "M83", "durationMs": 243000 },
      "playable": { "source": "soundcloud", "identifier": "1234567890", "title": "Midnight City", "author": "M83", "durationMs": 243000 },
      "instance": 7,
      "paused": false,
      "volume": 100,
      "filters": {},
      "positionMs": 61240,
      "anchorMs": 61240.0,
      "anchorSentAt": 1767225600100,
      "anchorAudibleAt": 1767225600100,
      "rate": 1.0,
      "sounding": true,
      "mixing": false,
      "deckBLoaded": false,
      "hosting": false
    }
  }
}
```

| `player` field | Meaning |
| --- | --- |
| `track` | What the bot asked for |
| `playable` | What is actually playing: differs from `track` when a Spotify/Apple/Deezer track was [mirrored](../sources.md#mirroring) |
| `instance` | Increments every time a new track starts on this player. Events carry it too |
| `positionMs` | Position of the last frame sent to Discord |
| `anchorMs`, `anchorSentAt`, `anchorAudibleAt`, `rate`, `sounding` | The [wire clock](../wire-clock.md) anchor |
| `mixing`, `mixInto`, `mixIntoPositionMs`, `mixProgress` | An active crossfade |
| `deckBLoaded`, `mixArmedAtMs` | A loaded deck B and an armed cue |
| `hosting` | A DJ voice-over is playing |
| `nextTrack` | A gapless next track is preloaded |

`ping` is the voice gateway heartbeat round trip in ms; `region` is derived
from the voice endpoint (`c-rotterdam09.discord.media` → `rotterdam`).

### `PATCH /v1/sessions/{session}/players/{guild}?noReplace=false`

Every field is optional; only what you send changes. The response is the
updated player (as above).

```json
{
  "track":     { "encoded": "eyJzb3…", "userData": { "requester": "1234" } },
  "position":  0,
  "endTime":   null,
  "paused":    false,
  "volume":    100,
  "filters":   { "timescale": { "speed": 1.1, "pitch": 1.0, "rate": 1.0 } },
  "voice":     { "token": "…", "endpoint": "c-rotterdam09.discord.media:443", "sessionId": "…", "channelId": "…", "bitrate": 128000 },
  "bitrate":   128000,
  "nextTrack": { "encoded": "eyJzb3…" },
  "analyze":   true
}
```

| Field | Effect |
| --- | --- |
| `track.encoded` | Play this track. `null` (or a `track` object with neither `encoded` nor `identifier`) **stops** the player (`TrackEndEvent` with `stopped`) |
| `track.identifier` | Lavalink v4: resolve this `loadtracks` identifier and play its first track (`400` if it finds nothing) |
| `track.userData` | Any JSON object, echoed back on the track in v4 events and players |
| `position` | With `track`: start position in ms. Alone: **seek** (ignored when nothing is playing, as in Cadence) |
| `paused` | With `track`: start paused. Alone: pause or resume (with `WAVYZON_PAUSE_FADE_MS` fade) |
| `volume` | `0`–`1000`, where `100` is unity gain. Clamped |
| `filters` | Replaces the whole filter set. See [Filters](../filters.md). Changes glide, they don't jump |
| `voice` | Discord voice credentials from `VOICE_SERVER_UPDATE` (`token`, `endpoint`) and `VOICE_STATE_UPDATE` (`sessionId`, `channelId`). `bitrate` is optional |
| `bitrate` | Opus bitrate in bps (16 000–384 000), remembered across voice updates |
| `nextTrack.encoded` | Preload the next track: it crossfades in over `WAVYZON_CROSSFADE_MS` before the current one ends, or starts on the **exact frame** it ends (gapless) when that is `0`. `null` clears it |
| `analyze` | Analyse the playing audio live (BPM, key, sections…), results at `/v1/mix/analysis/…` |

Query `noReplace=true`: ignore `track` if something is already playing.

Notes:

- `voice` and `track` can arrive **in any order**, in one request or several.
  A track sent before voice waits, without its clock moving.
- Sending the same voice state again (same `sessionId`, `token`, `endpoint`
  and `channelId`) while connected is a no-op (only a new `bitrate` is
  applied); a different one closes the old connection cleanly, waits for it
  to be gone, then dials the new one. An incomplete `voice` (a field
  missing) returns `400` and keeps the current connection.
- `endTime` is accepted for Lavalink compatibility and ignored, as in
  Cadence.
- An update that fails part-way (for example a valid `volume` with an
  unknown plugin filter) is not rolled back: fields are applied in the
  order `bitrate`, `analyze`, `filters`, `volume`, `voice`, `track`,
  `nextTrack`, `paused`, `position`, and the first error is returned.

Plugin filters go under `filters.pluginFilters`:

```json
{ "filters": { "reverb": { "mix": 0.25 }, "pluginFilters": { "bitcrush": { "bits": 6, "downsample": 2 } } } }
```

An invalid plugin filter configuration returns `400` with the plugin's
message.

### `DELETE /v1/sessions/{session}/players/{guild}`

Destroy the player and leave voice. `204 No Content`.

---

## Mixing

See [Mixing & crossfades](../mixing.md) for the concepts. All return `404` if
the player doesn't exist.

### `POST …/players/{guild}/mix/deckb`

Load and pre-buffer the incoming track on deck B.

```json
{ "encodedTrack": "eyJzb3…", "position": 32000 }
```

`200`: `{ "instance": 8 }`. `422` if the track can't be resolved or opened.

### `POST …/players/{guild}/mix/crossfade`

Start a transition now, or arm it at a cue on deck A.

```json
{
  "durationMs": 8000,
  "curve": "equal_power",
  "atPositionMs": 201500,
  "tempoRatio": 1.02,
  "gainRatio": 0.93,
  "profile": {
    "bassSwapAt": 0.5,
    "bassSwapRamp": 0.12,
    "sweepOutFromHz": 20,
    "sweepOutToHz": 400,
    "sweepInFromHz": 300,
    "sweepInToHz": 20
  }
}
```

`204` on success, `409` with no deck B or nothing on deck A, `400` for invalid
values.

### `POST …/players/{guild}/mix/host`

Play a short DJ voice-over (MP3, base64) over the transition, ducking the
music.

```json
{ "audio": "SUQzBAAAAAAAI1RTU0UAAAAPAAADTGF2ZjU4Lj…", "atPositionMs": 199000, "outFadeMs": 2500, "leadMs": 1800, "rampMs": 1200, "duckDb": 9 }
```

Limits: 512 KiB, 25 s. `204`, `409` while a track is still loading, `400` for
an invalid clip.

### `POST …/players/{guild}/mix/analyze`

Start live analysis on this player (same as `"analyze": true`). `204`.

### `DELETE …/players/{guild}/mix`

Cancel an armed transition and unload deck B. `204`.

### `GET /v1/mix/analysis/{source}/{identifier}`

The finished analysis ("fingerprint") of a track, or `404` if there is none
yet. URL-encode `identifier`.

```json
{
  "source": "soundcloud",
  "identifier": "1234567890",
  "durationMs": 243000,
  "rhythm": { "bpm": 105.0, "bpmConfidence": 0.91, "meter": 4, "beatsMs": [412, 983], "downbeatsMs": [412, 2698], "tempoMap": [ { "timeMs": 0, "bpm": 105.0 } ], "phrasesMs": [412, 18700], "phraseBars": 8 },
  "harmony": { "camelot": "8A", "key": "A", "scale": "minor", "confidence": 0.77 },
  "sections": [ { "type": "intro", "startMs": 0, "endMs": 18700, "energy": 0.31, "beatStart": 0, "beatEnd": 32 } ],
  "energy": [ { "timeMs": 0, "value": 0.2 } ],
  "onsets": [ { "timeMs": 412, "strength": 0.8 } ],
  "vocals": [ { "startMs": 41000, "endMs": 72000, "confidence": 0.84 } ],
  "mixRegions": { "introStartMs": 0, "introEndMs": 18700, "outroStartMs": 215000, "outroEndMs": 243000 },
  "loudness": { "integratedLufs": -8.9, "peakAmplitude": 0.98 },
  "vector": [0.12, 0.4],
  "analysisVersion": 3,
  "analyzerVersion": "0.1.0"
}
```

### `POST /v1/mix/analyze`

Queue offline analysis of a track.

```json
{ "encodedTrack": "eyJzb3…", "force": false }
```

`202`: `{ "queued": true }`. `queued` is `false` when a fingerprint already
exists (and `force` is `false`), the track failed recently, or the queue is
full.

---

## Lyrics

Plain-text lyrics from [lyrics.ovh](https://lyrics.ovh), in the response
shape of Lavalink's LavaLyrics plugin (so Lavalink clients can read them,
see [Lavalink compatibility](../lavalink.md)). Both routes also exist under
`/v4`. Off with `LYRICS_ENABLED=false`.

### `GET /v1/lyrics?track=…`

Lyrics of an encoded track (`encodedTrack` is accepted as well), looked up by
its artist and title.

```json
{
  "sourceName": "deezer",
  "provider": "lyrics.ovh",
  "text": "Pressure\nPushing down on me\n…",
  "lines": [],
  "plugin": {}
}
```

`204` when lyrics.ovh has none, `400` without `track`, `404` when lyrics are
disabled, `502` when lyrics.ovh failed. `lines` is always empty: lyrics.ovh
has no timed lyrics.

The track's own artist and title are tried first, then once more cleaned up
(first credited artist, title without `(feat. …)`, `- Remastered 2011`,
`(Official Video)`…), both at once. Answers are cached for 7 days, misses
for 6 hours. Expect about 5 s for a song lyrics.ovh does not know (it runs
its own fallback search), and well under a second otherwise.

### `GET /v1/sessions/{session}/players/{guild}/track/lyrics`

Same answer for the track the player is on; `404` when nothing is playing.

---

## Peer cache

Used between Wavyzon nodes (`WAVYZON_CACHE_PEERS`). Accept either the node
password or `WAVYZON_CACHE_SECRET`.

| Route | Response |
| --- | --- |
| `GET /v1/cache/index` | Array of cache keys (`<40 hex digits of SHA-256(source:identifier)>-<format>`) |
| `GET /v1/cache/file?key=…` | The cached audio (`application/octet-stream`, with `Content-Length`), or `404` |

`WAVYZON_CACHE_SECRET` is accepted only on these two routes.

---

## Route summary

| Method | Route |
| --- | --- |
| `GET` | `/v1/websocket` ([WebSocket](websocket.md)) |
| `GET` | `/v1/info`, `/v1/stats`, `/v1/clock`, `/metrics` |
| `GET` | `/v1/plugins` |
| any | `/v1/plugins/{name}/…` |
| `GET` | `/v1/loadtracks`, `/v1/decodetrack` |
| `POST` | `/v1/decodetracks`, `/v1/prefetch` |
| `PATCH` | `/v1/sessions/{session}` |
| `GET` | `/v1/sessions/{session}/players` |
| `GET` `PATCH` `DELETE` | `/v1/sessions/{session}/players/{guild}` |
| `POST` | `…/players/{guild}/mix/deckb`, `…/mix/crossfade`, `…/mix/host`, `…/mix/analyze` |
| `DELETE` | `…/players/{guild}/mix` |
| `GET` | `/v1/mix/analysis/{source}/{identifier}` |
| `POST` | `/v1/mix/analyze` |
| `GET` | `/v1/lyrics`, `/v1/sessions/{session}/players/{guild}/track/lyrics` |
| `GET` | `/v1/cache/index`, `/v1/cache/file` |
