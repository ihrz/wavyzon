# Examples

Ready-to-copy recipes for a bot. The TypeScript ones use
[`lavalink-client`](https://github.com/Tomato6966/lavalink-client) with
discord.js (set up as in the [Quick start](quick-start.md)); the same JSON
works from any language through the [REST API](api/rest.md). `player` is a
lavalink-client player and `PASS` the node password.

## Search and play

```ts
// Plain text follows WAVYZON_SEARCH_ORDER (Deezer first by default).
let res = await player.search({ query: "daft punk one more time" }, user);

// A platform prefix searches one source.
res = await player.search({ query: "scsearch:lofi beats" }, user);   // SoundCloud
res = await player.search({ query: "dzsearch:uet luther" }, user);   // Deezer

// Links: Spotify / Apple Music / Deezer tracks, albums and playlists,
// SoundCloud, direct audio files and Icecast/Shoutcast radios.
res = await player.search({ query: "https://open.spotify.com/track/0dEIca2nhcxDUV8C5QkPYb" }, user);
res = await player.search({ query: "https://ice1.somafm.com/groovesalad-128-mp3" }, user);

await player.queue.add(res.loadType === "playlist" ? res.tracks : res.tracks[0]);
if (!player.playing) await player.play();
```

Spotify and Apple Music only give metadata: Wavyzon plays the same recording
from a playback source, matched by ISRC (or title and duration). A track with
no faithful match fails with `no matching recording` rather than playing a
remix or another song.

## Smooth transitions

Tell the node what comes next and it pre-buffers it, then crossfades into it
(3 s by default, `WAVYZON_CROSSFADE_MS`) or chains it gaplessly (`0`):

```ts
// After each track starts, hand the next one of your queue to the node.
client.lavalink.on("trackStart", async (player) => {
  const next = player.queue.tracks[0];
  await player.node.updatePlayer({
    guildId: player.guildId,
    playerOptions: { nextTrack: next ? { encoded: next.encoded } : { encoded: null } },
  });
});
```

Your queue logic doesn't change: when the fade completes you get the usual
`trackEnd` (`finished`), and playing the next track adopts the one already
audible instead of restarting it. Manual crossfades, DJ voice-overs and
beat-matched transitions are in [Mixing & crossfades](mixing.md).

## Filters

Built-in filters use Lavalink's shapes, plus Wavyzon's reverb:

```ts
await player.node.updatePlayer({
  guildId: player.guildId,
  playerOptions: {
    filters: {
      // Bass boost: bands 0–14, gain −0.25..1.0
      equalizer: [
        { band: 0, gain: 0.25 },
        { band: 1, gain: 0.2 },
        { band: 2, gain: 0.1 },
      ],
      // Nightcore
      timescale: { speed: 1.2, pitch: 1.2, rate: 1.0 },
      // Room: mix, room size and damping, 0–1
      reverb: { mix: 0.15, room: 0.4, damping: 0.6 },
    },
  },
});
```

Filters glide to their new values instead of jumping, and `filters: {}`
turns everything off. Every field: [Filters](filters.md).

## Equalizer APO presets

The [`apo` plugin filter](apo.md) plays through Equalizer APO
configurations. Inline text:

```ts
const config = [
  "Preamp: -6 dB",
  "Filter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.70",
  "Filter 2: ON PK Fc 3000 Hz Gain -2.0 dB Q 1.41",
  "Reverb: Mix 10 % Room 35 % Damping 65 %",
].join("\n");

await player.node.updatePlayer({
  guildId: player.guildId,
  playerOptions: { filters: { pluginFilters: { apo: { config } } } },
});
```

Or a preset uploaded to the node's `data/plugins/apo/` folder:

```ts
filters: { pluginFilters: { apo: { preset: "headphones/hd600" } } }
```

Validate a user's file before applying it (a `400` explains what's wrong):

```ts
const res = await fetch("http://localhost:2333/v1/plugins/apo/check", {
  method: "POST",
  headers: { Authorization: PASS, "Content-Type": "application/json" },
  body: JSON.stringify({ config: await attachmentText() }),
});
const body = await res.json(); // { ok, stages, warnings } or { ok: false, error }
```

`404` on that route means the node is not Wavyzon or was built without the
plugin.

## Synced lyrics

`TrackAudibleEvent` is sent when the first packet of a track left the
socket, with `audibleAt` (Unix ms): the moment listeners start hearing it.
Enable Wavyzon's extra events on a v4 session, then anchor your lyrics on it:

```ts
client.lavalink.nodeManager.on("connect", async (node) => {
  await node.request(`/sessions/${node.sessionId}`, (r) => {
    r.method = "PATCH";
    r.headers!["Content-Type"] = "application/json";
    r.body = JSON.stringify({ wavyzonEvents: true });
  });
});

client.lavalink.nodeManager.on("raw", (node, payload) => {
  if (payload.op === "event" && payload.type === "TrackAudibleEvent") {
    // Position heard right now = payload.positionMs + (Date.now() - payload.audibleAt)
    lyrics.start(payload.guildId, payload.positionMs, payload.audibleAt);
  }
});
```

The lyrics text itself (lyrics.ovh, LavaLyrics shape):

```bash
curl -s -H "Authorization: $PASS" \
  "http://localhost:2333/v1/sessions/$SESSION/players/$GUILD/track/lyrics" | jq -r .text
```

Why and how precise: [The wire clock](wire-clock.md).

## Useful requests

```bash
# Node status, sources, plugins
curl -s -H "Authorization: $PASS" http://localhost:2333/v1/info | jq

# Every player of a session (position, volume, filters…)
curl -s -H "Authorization: $PASS" http://localhost:2333/v1/sessions/$SESSION/players | jq

# Decode an encoded track
curl -s -H "Authorization: $PASS" "http://localhost:2333/v1/decodetrack?encodedTrack=$TRACK" | jq

# APO presets loaded on the node, and reload them after an upload
curl -s -H "Authorization: $PASS" http://localhost:2333/v1/plugins/apo/ | jq '.presets[].name'
curl -s -X POST -H "Authorization: $PASS" http://localhost:2333/v1/plugins/apo/reload | jq '.warnings'

# Prometheus metrics (players, stalls, sources, cache…)
curl -s -H "Authorization: $PASS" http://localhost:2333/metrics | grep '^wavyzon_players'
```

## Your own plugin

A filter in a few lines of Rust, compiled into the node and usable as
`pluginFilters.gain`:

```rust
use wavyzon_plugin::{serde_json::Value, AudioFilter, Failure, FilterFactory};

struct Gain(f32);

impl AudioFilter for Gain {
    fn process(&mut self, frame: &mut [f32]) {
        frame.iter_mut().for_each(|s| *s *= self.0);
    }
}

struct GainFactory;

impl FilterFactory for GainFactory {
    fn name(&self) -> &str {
        "gain"
    }
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        let db = config["db"].as_f64().unwrap_or(0.0);
        Ok(Box::new(Gain(10f32.powf(db as f32 / 20.0))))
    }
}
```

Registering it, sources, event listeners and HTTP routes:
[Writing plugins](plugins.md).
