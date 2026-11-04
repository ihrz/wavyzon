# WebSocket & events

The WebSocket carries everything the node tells your bot: the session id,
periodic player state, and events. Your bot never *sends* anything on it;
commands go through the [REST API](rest.md).

## Connecting

```http
GET /v1/websocket HTTP/1.1
Upgrade: websocket
Authorization: <WAVYZON_PASSWORD>
User-Id: 945202900907470899
Client-Name: my-bot/1.0
Session-Id: 8e1f0c2a9b7d4e61      (optional, to resume)
```

| Header | Required | Meaning |
| --- | --- | --- |
| `Authorization` | yes | The node password |
| `User-Id` | yes | Your bot's user id (used when joining voice). `400` without it |
| `Client-Name` | no | Shown in logs |
| `Session-Id` | no | Resume this session, if it still exists, belongs to the same `User-Id` and was opened on the same protocol (`/v1` or `/v4`) |

The node pings every 30 s; a client that stops answering is disconnected.
Every write has a 10 s timeout.

## Messages

Every message is a JSON object with an `op`.

### `ready`

First message on every connection.

```json
{ "op": "ready", "resumed": false, "sessionId": "8e1f0c2a9b7d4e61" }
```

If `resumed` is `true`, your players kept running while you were away, and
every message you missed follows immediately, in order (up to 2 000 messages;
beyond that the oldest are dropped and a warning is logged).

### `playerUpdate`

Sent every `WAVYZON_UPDATE_INTERVAL_MS` (default 1 s) while a player is
playing, and right after every event of that player.

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

The fields of `state.player` are described in
[REST › Players](rest.md#get-v1sessionssessionplayersguild). Use the anchor to
compute a smooth live position (see [the wire clock](../wire-clock.md)); don't
interpolate from `positionMs`.

### `event`

```json
{ "op": "event", "type": "TrackStartEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7 }
```

Common fields:

| Field | Meaning |
| --- | --- |
| `type` | Event name, see below |
| `guildId` | The player |
| `track` | The track the event is about (decoded, same shape as `info` in REST) |
| `encoded` | Its encoded form, when relevant |
| `instance` | The play instance. **Compare it to the instance you started** and ignore events from a track you already replaced |

Optional fields are **omitted** when they don't apply (never `null`, never `0`).

## Events

### Track lifecycle

A normal play produces, in order:

```text
TrackLoadingEvent ─► TrackStartEvent ─► TrackAudibleEvent ─► … ─► TrackEndEvent{finished}
```

#### `TrackLoadingEvent`

The track was accepted and is being resolved (mirror lookup, stream URL).

```json
{ "op": "event", "type": "TrackLoadingEvent", "guildId": "1046781946263277578", "track": { "source": "spotify", "identifier": "0dEIca2nhcxDUV8C5QkPYb", "title": "Midnight City", "author": "M83", "durationMs": 243960, "isrc": "FR6V81141061" }, "encoded": "eyJzb3…", "instance": 7 }
```

#### `TrackStartEvent`

Decoding has begun. Audio is not audible yet: use `TrackAudibleEvent` for
anything time-sensitive.

```json
{ "op": "event", "type": "TrackStartEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7 }
```

#### `TrackAudibleEvent`

**The first packet of the track left the socket.** Anchor lyrics, progress
bars and "now playing" timestamps here.

```json
{ "op": "event", "type": "TrackAudibleEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7, "positionMs": 0, "sentAt": 1767225601040, "audibleAt": 1767225601040 }
```

| Field | Meaning |
| --- | --- |
| `positionMs` | Track position of that first packet (non-zero after a start position or seek) |
| `sentAt` | Node time (Unix ms) the packet was written to the UDP socket |
| `audibleAt` | `sentAt + WAVYZON_PLAYOUT_DELAY_MS`: when listeners hear it |

#### `TrackEndEvent`

```json
{ "op": "event", "type": "TrackEndEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7, "reason": "finished" }
```

| `reason` | Meaning | Start the next track? |
| --- | --- | --- |
| `finished` | Played to the end | yes |
| `loadFailed` | Could not be resolved or opened (a `TrackExceptionEvent` precedes it) | yes |
| `stopped` | Stopped by the client (`track.encoded: null`) | no |
| `replaced` | A new track replaced it, or a crossfade promoted deck B | no |
| `cleanup` | The player or session was destroyed | no |

#### `TrackExceptionEvent`

```json
{ "op": "event", "type": "TrackExceptionEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7, "exception": { "severity": "common", "message": "no playable match", "cause": "no SoundCloud result within 5 s of 243960 ms", "source": "soundcloud" } }
```

`severity` is `common`, `suspicious` or `fault` (see
[REST › loadtracks](rest.md#get-v1loadtracksidentifier)). An exception during
playback may or may not end the track; an exception during loading is
followed by `TrackEndEvent{loadFailed}`. A connection that breaks
mid-track is reopened where it stopped (up to three times) before any
`stream interrupted` exception is sent.

#### `TrackStuckEvent`

No audio for 10 s while the track was not finished (a stalled source).

```json
{ "op": "event", "type": "TrackStuckEvent", "guildId": "1046781946263277578", "track": { … }, "encoded": "eyJzb3…", "instance": 7, "thresholdMs": 10000 }
```

#### `PositionEvent`

A seek moved the clock. Re-anchor your position display.

```json
{ "op": "event", "type": "PositionEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 7, "positionMs": 120000 }
```

### Mixing

See [Mixing & crossfades](../mixing.md).

#### `MixArmedEvent`

A crossfade was armed at a cue on deck A. `track` is deck B's track.

```json
{ "op": "event", "type": "MixArmedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 8, "positionMs": 201500, "durationMs": 8000 }
```

#### `MixStartedEvent`

The fade began. `track` is the outgoing track; `into` is the incoming one and
`intoPositionMs` its position at the start of the fade.

```json
{ "op": "event", "type": "MixStartedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 7, "into": { … }, "intoPositionMs": 32000, "durationMs": 8000 }
```

#### `MixPromotedEvent`

The fade completed; deck B is now the main track. Followed by
`TrackEndEvent{replaced}` for the old track and `TrackStartEvent` for the new.

```json
{ "op": "event", "type": "MixPromotedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 7, "into": { … }, "intoPositionMs": 40000 }
```

#### `MixDroppedEvent`

Deck B was discarded: cancelled, failed to load, or deck A ended first.

```json
{ "op": "event", "type": "MixDroppedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 8 }
```

#### `HostStartedEvent` / `HostEndedEvent`

A DJ voice-over (`POST …/mix/host`) started or ended. On start, `durationMs` is
the clip length; on end, the time it actually played.

```json
{ "op": "event", "type": "HostStartedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 7, "into": { … }, "intoPositionMs": 30000, "durationMs": 6200 }
```

```json
{ "op": "event", "type": "HostEndedEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 8, "durationMs": 6200 }
```

### Voice and transport

#### `SendStallEvent`

The send loop fell behind its 20 ms schedule by `stallMs`. Occasional small
stalls are harmless; frequent ones mean the host is overloaded.

```json
{ "op": "event", "type": "SendStallEvent", "guildId": "1046781946263277578", "track": { … }, "instance": 7, "stallMs": 64 }
```

#### `WebSocketClosedEvent`

Discord's voice gateway closed the connection.

```json
{ "op": "event", "type": "WebSocketClosedEvent", "guildId": "1046781946263277578", "code": 4014, "closeReason": "Disconnected", "terminal": true }
```

| Field | Meaning |
| --- | --- |
| `code` | Voice gateway close code (4006 session invalid, 4014 kicked / channel deleted, 4015 server crashed, …) |
| `closeReason` | Discord's reason text |
| `terminal` | `true` if Wavyzon will **not** reconnect by itself; send fresh voice credentials (or destroy the player) |

Cadence's `/v1` sent `reason` and `byRemote`; Wavyzon's `/v1` names the
text `closeReason` (because `reason` is the `TrackEndEvent` reason) and has
no `byRemote`. On `/v4` the Lavalink names are used: `code`, `reason`,
`byRemote` (always `true`), plus `terminal`.

## Event summary

| Event | Extra fields | Sent on `/v4` by default |
| --- | --- | --- |
| `TrackLoadingEvent` | | no |
| `TrackStartEvent` | | **yes** |
| `TrackAudibleEvent` | `positionMs`, `sentAt`, `audibleAt` | no |
| `TrackEndEvent` | `reason` | **yes** |
| `TrackExceptionEvent` | `exception` | **yes** |
| `TrackStuckEvent` | `thresholdMs` | **yes** |
| `PositionEvent` | `positionMs` | no |
| `MixArmedEvent` | `positionMs`, `durationMs` | no |
| `MixStartedEvent` | `into`, `intoPositionMs`, `durationMs` | no |
| `MixPromotedEvent` | `into`, `intoPositionMs` | no |
| `MixDroppedEvent` | | no |
| `HostStartedEvent` | `into`, `intoPositionMs`, `durationMs` | no |
| `HostEndedEvent` | `durationMs` | no |
| `SendStallEvent` | `stallMs` | no |
| `WebSocketClosedEvent` | `code`, `closeReason`/`reason`, `terminal` | **yes** |

Plugins receive every one of these events through
[`EventListener`](../plugins.md#4-event-listeners), after the client got it.

## A minimal client

```js
import WebSocket from "ws";

const ws = new WebSocket("ws://localhost:2333/v1/websocket", {
  headers: { Authorization: process.env.WAVYZON_PASSWORD, "User-Id": BOT_ID, "Client-Name": "demo/1.0" },
});

let sessionId;
const current = new Map(); // guildId → instance we started

ws.on("message", (raw) => {
  const msg = JSON.parse(raw);
  switch (msg.op) {
    case "ready":
      sessionId = msg.sessionId;
      break;
    case "playerUpdate":
      // update progress bars from msg.state.player's anchor
      break;
    case "event":
      if (msg.instance && current.get(msg.guildId) !== undefined && msg.instance < current.get(msg.guildId)) return; // stale
      if (msg.type === "TrackStartEvent") current.set(msg.guildId, msg.instance);
      if (msg.type === "TrackEndEvent" && ["finished", "loadFailed"].includes(msg.reason)) playNext(msg.guildId);
      break;
  }
});
```
