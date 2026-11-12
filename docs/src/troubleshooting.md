# Troubleshooting & FAQ

## Startup

**`WAVYZON_PASSWORD must be set to a non-default value`**
Set `WAVYZON_PASSWORD` to something other than empty or `youshallnotpass`.

**`address already in use`**
Another process (often a Lavalink you forgot) listens on 2333. Stop it or
change `WAVYZON_ADDRESS`.

**`plugin X targets API vN, this node speaks vM`**
The plugin was built against another `wavyzon-plugin` major. Update the
plugin. See [Versioning](plugins.md#versioning).

**A plugin in `WAVYZON_PLUGINS` is "not compiled in"**
Plugins are enabled at **build** time with a cargo feature, then selected at
runtime. Rebuild with the plugin's feature (see
[Enabling a plugin](plugins.md#enabling-a-plugin-in-the-node)).

**Build fails in `audiopus_sys` / CMake**
Install CMake and a C compiler. With CMake 4, the repository's
`.cargo/config.toml` already sets `CMAKE_POLICY_VERSION_MINIMUM=3.5`; if you
build from another directory or override `CARGO_HOME` config, set it
yourself. To use the system libopus instead: `OPUS_STATIC=0` and install
libopus with its `pkg-config` file.

## Connecting

**`401` on every request**
The header is `Authorization: <password>` with the raw password, no
`Bearer`. Check for trailing newlines in your env file.

**WebSocket returns `400 User-Id header is required`**
Send your bot's user id in `User-Id`. Lavalink clients do it after they know
the bot user: initialise the manager **after** the client is ready.

**Resuming doesn't resume**
The `Session-Id` must belong to the same `User-Id`, be opened on the same
protocol (`/v1` vs `/v4`) and reconnect within the timeout. Resuming must
have been enabled with `PATCH /v1/sessions/{id}` *before* the disconnect.

## Playback

**The bot joins but there is no sound**

1. Check the player: `GET /v1/sessions/{s}/players/{g}`. `state.connected`
   must be `true`.
2. If `connected` is `false`: the voice update is missing or stale. Forward
   **both** `VOICE_STATE_UPDATE` (session id, channel id) and
   `VOICE_SERVER_UPDATE` (token, endpoint) and send them together in
   `voice`. Lavalink clients do this if you forward raw gateway events
   (`client.on("raw", …)`).
3. If `connected` is `true` but `sounding` is `false`: the track is still
   loading, buffering, or DAVE is negotiating (see below). Look for
   `TrackExceptionEvent`.
4. Outbound UDP must be allowed from the node to Discord (ports 50000–65535).

**`TrackExceptionEvent: no playable match`**
A Spotify/Apple/Deezer track found no recording on your
`WAVYZON_PLAYBACK_SOURCES`. Add a searchable playback source (a plugin) or
play a direct source (an `http` link or a `local:` file).

**`ytsearch:` returns odd results or nothing**
Wavyzon has no YouTube source: `ytsearch:`/`ytmsearch:` are plain searches
through `WAVYZON_SEARCH_ORDER`, so they return whatever the first source in
that order finds (or nothing if every source is down). Use `scsearch:` or
another explicit prefix, and set your client's default search platform
accordingly.

**A radio stream stops after a while**
Some Icecast servers close idle or slow connections. Check
`TrackExceptionEvent` for the cause and whether the station enforces a
per-IP limit. Radio can't be seeked, and has no duration.

**`HTTP source: address is private`**
The URL resolves to a private or loopback address and
`WAVYZON_HTTP_ALLOW_PRIVATE` is `false`. That's on purpose (it protects your
network). Serve the file from a public address, use the
[local source](sources.md#local-files), or enable the flag on a trusted node.

**Seeking an HTTP file is slow**
The server doesn't support `Range` requests, or the file layout prevents
computing an exact offset (e.g. VBR MP3 without a seek table); Wavyzon
decodes from the start to stay sample-accurate. Run with
`RUST_LOG=info,wavyzon_sources=debug` to see how each open was served.

**Audio stutters**
Look at `SendStallEvent` and `wavyzon_send_stall_ms_total`. Stalls mean the
process isn't getting CPU on time: an overloaded host, a noisy neighbour, CPU
throttling in a container (`cpu.max`), or swap. Give the container at least
one full core and avoid tight CPU limits.

**`TrackStuckEvent`**
No audio for 10 s: the source stopped sending data. Usually a network issue
between the node and the source; the bot should skip.

## Voice

| Close code | Meaning | What Wavyzon does |
| --- | --- | --- |
| 4006 | Session no longer valid | Reconnects with a fresh identify if it can, else `terminal: true` |
| 4009 | Session timed out | Reconnects |
| 4014 | Disconnected (kicked, channel deleted, moved) | `terminal: true`: the bot must rejoin or destroy the player |
| 4015 | Voice server crashed | Resumes |
| 4016 / 4017 | Unknown encryption mode / E2EE required | Shouldn't happen: Wavyzon supports the current AEAD modes and DAVE. Report it |

> Note: provisional. The reconnect policy per close code; `terminal` in the
> event is authoritative.

**Silence right after joining an E2EE call**
With DAVE, audio is held until the MLS epoch is ready, never sent
unencrypted. That's normally well under a second. A hold over 15 s restarts
the voice session automatically.

## FAQ

**Does Wavyzon play YouTube?**
No, and there is no plan to add it in core. A plugin could add a source.

**Does Wavyzon play Deezer?**
Yes: search, links, ISRC lookups out of the box, and full-length streams when
`DEEZER_ARL` and `DEEZER_MASTER_KEY` are set (see
[Configuration](configuration.md)). Without them Deezer is metadata only and
its tracks are mirrored to a playback source.

**Can I use Lavalink plugins (LavaSrc, LavaSearch…)?**
No: they are JVM code. Wavyzon covers the common LavaSrc features
(Spotify/Apple/Deezer links mirrored by ISRC) natively, and has its own
[plugin API](plugins.md).

**Can I play tracks encoded by Lavalink?**
No: re-resolve them with `loadtracks`. Wavyzon's own encoded tracks work on
any Wavyzon node.

**Is it production ready?**
The design and protocol come from Cadence, which runs in production. Wavyzon
itself is at 0.1; test it with your bot before switching a large fleet.

**Can I use it commercially?**
Wavyzon is licensed under CC BY-NC-SA 4.0, which **does not allow commercial
use**. Contact the iHorizon team if you need other terms.

**Windows / macOS?**
Yes, for development. Production deployments are tested on Linux.

## Checking the sources with `livecheck`

The `livecheck` binary (built next to `wavyzon`) runs the real source path
against the live services, with the same environment as the node:

```bash
cargo build --release -p wavyzon --bin livecheck
./target/release/livecheck                       # search, mirror, open and decode one track
./target/release/livecheck "scsearch:midnight city"
./target/release/livecheck -bench "q one" "q two"   # search / open / first-audio latency
./target/release/livecheck -seek                 # seeks compared with a full decode
./target/release/livecheck -rec "dzsearch:m83"   # recommendations for the first result
./target/release/livecheck -lyrics "Queen - Under Pressure"   # lyrics chain: which provider answers
```

It exits non-zero, with the reason, when something fails (nothing found, no
playable match, silence or noise instead of music).
