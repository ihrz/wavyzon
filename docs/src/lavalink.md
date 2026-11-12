# Lavalink v4 compatibility

Wavyzon speaks **Lavalink's v4 protocol** next to its own `/v1`. A bot built
on a Lavalink client library can point at Wavyzon and play, with no code
changes beyond the node address and password.

## What is supported

| Lavalink v4 | Wavyzon |
| --- | --- |
| `GET /v4/websocket` (`Authorization`, `User-Id`, `Client-Name`, `Session-Id`) | ✅ `ready`, `playerUpdate`, `stats` (every minute), `event` |
| `GET /version` | ✅ plain-text version |
| `GET /v4/info` | ✅ with `sourceManagers`, `filters`, `plugins` |
| `GET /v4/stats` | ✅ |
| `GET /v4/loadtracks` | ✅ Lavalink result shapes |
| `GET /v4/decodetrack`, `POST /v4/decodetracks` | ✅ |
| `PATCH /v4/sessions/{id}` | ✅ `resuming`, `timeout` (+ Wavyzon's extra-events switch) |
| `GET /v4/sessions/{id}/players`, `GET`/`PATCH`/`DELETE …/players/{guild}` | ✅ including `noReplace`, `userData`, `track.identifier` (`endTime` is accepted and ignored) |
| Standard filters | ✅ `equalizer`, `timescale`, `karaoke`, `lowPass`, `tremolo`, `vibrato`, `rotation`, plus `reverb` |
| `pluginFilters` | ✅ routed to [plugin filters](filters.md#plugin-filters) |
| LavaLyrics `GET /v4/lyrics`, `GET …/players/{guild}/track/lyrics` | ✅ lyrics from [Genius](genius-lyrics.md), LRCLIB (with timed `lines`) or lyrics.ovh, order set by `WAVYZON_LYRICS_ORDER`; `lavalyrics-plugin` is listed in `/v4/info` `plugins` |
| LavaSrc's Flowery TTS (`ftts://…`) | ✅ [`flowery` plugin](flowery-tts.md), on by default; `ftts` and `flowery-tts` are listed in `sourceManagers` |
| `/v4/routeplanner/*` | ❌ (no IP rotation) |
| `channelMix`, `distortion` filters | ❌ not listed in `/v4/info` `filters`; ignored |
| Lavalink (JVM) plugins | ❌ Wavyzon has its [own plugin API](plugins.md) |
| YouTube | ❌ see below |

Wavyzon additions also reachable under `/v4`: `POST /v4/prefetch`,
`…/players/{guild}/mix/*`, `GET /v4/mix/analysis/{source}/{identifier}`,
`POST /v4/sessions/{id}/mix/analyzetrack`, and the extra player fields
`bitrate` and `nextTrack`.

**Lyrics**: Wavyzon lists `lavalyrics-plugin` but no lyrics *source* plugin
(it is not LavaSrc), so clients that validate plugins before calling lyrics,
like `lavalink-client`'s `node.lyrics.get()`, refuse to. Call the route
directly instead (`node.request("/lyrics?track=…")`), or turn the client's
plugin validation off.

## Differences you may notice

**Track shapes.** On `/v4` tracks have Lavalink's form:

```json
{
  "encoded": "eyJzb3…",
  "info": {
    "identifier": "1234567890",
    "isSeekable": true,
    "author": "M83",
    "length": 243000,
    "isStream": false,
    "position": 0,
    "title": "Midnight City",
    "uri": "https://soundcloud.com/m83/midnight-city",
    "artworkUrl": "https://i1.sndcdn.com/artworks-000-t500x500.jpg",
    "isrc": "FR6V81141061",
    "sourceName": "soundcloud"
  },
  "pluginInfo": {},
  "userData": {}
}
```

The `encoded` string is Wavyzon's (base64url JSON), **not** Lavaplayer's
binary format. Encoded tracks from a real Lavalink node can't be played by
Wavyzon and vice versa: re-resolve them with `loadtracks`. Clients that decode
tracks locally (rare) must call `/v4/decodetrack` instead.

**Playlists** carry `pluginInfo.totalTracks`, `pluginInfo.url` and
`pluginInfo.artworkUrl`, the convention of LavaSrc.

**Errors** on `loadType: "error"` use Lavalink's exception shape
(`message`, `severity`, `cause`, `causeStackTrace` (always empty)).

**`/v4/info`** reports `"jvm"` as the Rust version and `"lavaplayer"` as
`wavyzon <version>`, and adds `"wavyzon": "<version>"`, `"wireClock": true`,
`"mixing": true`, `"dave"` and `"analysis"` so a client can detect Wavyzon.
Cadence's `"cadence": "<version>"` field is kept too, so bots that detect
Cadence keep working. `plugins` lists Wavyzon plugins as `{name, version}`.

**`sourceManagers`** lists Wavyzon's sources plus the alias `youtube`. Some
clients refuse to search unless the node *claims* YouTube; the alias keeps
them happy. Wavyzon has no YouTube source, so a `ytsearch:` (or
`ytmsearch:`) query is treated as a plain search: it runs through
`WAVYZON_SEARCH_ORDER` like text without a prefix. **Set your client's
default search platform to `scsearch`** (or
`spsearch` / `dzsearch` / `amsearch` if you want metadata searches that are
then [mirrored](sources.md#mirroring)).

**Positions** in v4 `playerUpdate` come from the wire clock, so they are
accurate, but v4 has no field for the anchor. If you want lyrics-grade
timing, use the extra events below or the native `/v1` protocol.

## Extra events

By default a v4 session only receives Lavalink's standard events
(`TrackStartEvent`, `TrackEndEvent`, `TrackExceptionEvent`,
`TrackStuckEvent`, `WebSocketClosedEvent`), so strict clients aren't
confused. To also receive Wavyzon's (`TrackAudibleEvent`, `PositionEvent`,
mix and host events, `SendStallEvent`):

```http
PATCH /v4/sessions/{sessionId}
Authorization: <password>
Content-Type: application/json

{ "wavyzonEvents": true }
```

Check `/v4/info` for `"wireClock": true` first, so the same bot still works
against a plain Lavalink node.

Cadence's `cadenceEvents` is accepted as an alias of `wavyzonEvents`.

## Client setup

### lavalink-client (JavaScript / TypeScript)

[`lavalink-client`](https://github.com/Tomato6966/lavalink-client) works as-is.

```ts
import { Client, GatewayIntentBits } from "discord.js";
import { LavalinkManager } from "lavalink-client";

const client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates] });

client.lavalink = new LavalinkManager({
  nodes: [
    {
      id: "wavyzon",
      host: "localhost",
      port: 2333,
      authorization: process.env.WAVYZON_PASSWORD!,
      secure: false, // true behind TLS (wss/https)
    },
  ],
  sendToShard: (guildId, payload) => client.guilds.cache.get(guildId)?.shard?.send(payload),
  client: { id: process.env.CLIENT_ID!, username: "my-bot" },
  autoSkip: true,
  playerOptions: {
    defaultSearchPlatform: "scsearch", // Wavyzon has no YouTube
    onDisconnect: { autoReconnect: true, destroyPlayer: false },
  },
});

client.on("raw", (d) => client.lavalink.sendRawData(d));
client.once("ready", () => client.lavalink.init({ id: client.user!.id, username: client.user!.username }));

// play something
const player = client.lavalink.createPlayer({ guildId, voiceChannelId, textChannelId, selfDeaf: true, volume: 100 });
await player.connect();
const res = await player.search({ query: "https://example.com/radio.mp3" }, interaction.user);
await player.queue.add(res.tracks[0]);
if (!player.playing) await player.play();
```

Wavyzon's own filters and plugin filters go through the raw update:

```ts
await player.node.updatePlayer({
  guildId: player.guildId,
  playerOptions: {
    filters: {
      reverb: { mix: 0.3, room: 0.85, damping: 0.4 },
      pluginFilters: { bitcrush: { bits: 6 } },
    },
  },
});
```

To get `TrackAudibleEvent` and friends, enable extra events once the node is
connected and listen to raw node messages:

```ts
client.lavalink.nodeManager.on("connect", async (node) => {
  if (node.info?.wireClock) {
    await node.request(`/sessions/${node.sessionId}`, (r) => {
      r.method = "PATCH";
      r.headers!["Content-Type"] = "application/json";
      r.body = JSON.stringify({ wavyzonEvents: true });
    });
  }
});
client.lavalink.nodeManager.on("raw", (node, payload) => {
  if (payload.op === "event" && payload.type === "TrackAudibleEvent") {
    lyrics.anchor(payload.guildId, payload.positionMs, payload.audibleAt);
  }
});
```

> The exact helper names (`node.request`, the `raw` event) depend on your
> lavalink-client version; any way of sending a `PATCH` to the session works.

### Shoukaku, Moonlink.js, Riffy, Kazagumo

Configure the node URL and password as you would for Lavalink:

```js
// Shoukaku
const shoukaku = new Shoukaku(new Connectors.DiscordJS(client), [
  { name: "wavyzon", url: "localhost:2333", auth: process.env.WAVYZON_PASSWORD },
]);
```

Use `scsearch:` (or a full URL) in `loadtracks` queries instead of
`ytsearch:`.

### Lavalink.py, Wavelink, Mafic (Python)

```python
import wavelink

node = wavelink.Node(uri="http://localhost:2333", password=os.environ["WAVYZON_PASSWORD"])
await wavelink.Pool.connect(nodes=[node], client=bot)

tracks = await wavelink.Playable.search("Midnight City", source="scsearch")
```

### JDA / Lavalink-Client (Java/Kotlin), DisGo (Go), Lavalink4NET (C#)

All point at a v4 node with host, port and password; nothing else changes.
Avoid features that rely on Lavaplayer's binary track format.

## When to move to `/v1`

Stay on `/v4` if you just want playback. Move to `/v1` (or enable extra
events) when you need:

- the wire-clock anchor in every `playerUpdate`, for synced lyrics;
- `TrackLoadingEvent` / `TrackAudibleEvent` to show accurate loading and
  start times;
- the `playable` track (what a Spotify link was mirrored to).

See [Migrating from Lavalink](guides/from-lavalink.md) for a step-by-step guide.
