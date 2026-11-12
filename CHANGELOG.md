# Version 0.2.0 (Deezer streaming, crossfades, lyrics and three new plugins)

---

## 🎧 Deezer streams again

Deezer is back as a **playback source**: log in with one or more account cookies (`DEEZER_ARL`, with `failover`, `round-robin` or `random` rotation) and `DEEZER_MASTER_KEY`, and Wavyzon streams and decrypts Deezer tracks itself, with **instant seeks** on constant-bitrate files. Without them, Deezer stays metadata-only and its tracks are matched to another playback source, as in 0.1.0. Recommendations use the logged-in track mix again.

> **Changed defaults**: `WAVYZON_PLAYBACK_SOURCES` is now `deezer,soundcloud` and `WAVYZON_SEARCH_ORDER` starts with `deezer`. Set them back to `soundcloud` to keep 0.1.0's behaviour.

---

## 🔀 Automatic crossfades

When the bot queues a `nextTrack`, the current track now **fades into it** `WAVYZON_CROSSFADE_MS` before its end (default 3 s, equal-power) instead of cutting over on the last frame. Bots see the same events as a gapless swap. `0` keeps strict gapless playback.

---

## 🎤 Lyrics: Genius, then lyrics.ovh

`/v1/lyrics` and `/v4/lyrics` serve plain lyrics in the shape of Lavalink's **LavaLyrics** plugin, and `/v4/info` lists `lavalyrics-plugin`. The new `genius` plugin, **on by default**, asks [Genius](https://genius.com) first; when Genius has none or fails, lyrics.ovh answers (Genius off: lyrics.ovh directly). Set `WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN` (or `_CLIENT_ID` + `_CLIENT_SECRET`) for reliable searches; the `provider` field says which one answered. `livecheck -lyrics "<artist> - <title>"` checks the chain live.

---

## 🗣️ Flowery text-to-speech

The new `flowery` plugin, **on by default**, makes the bot speak with [Flowery](https://flowery.pw)'s free TTS. It reads LavaSrc's `ftts://<text>?voice=…&speed=…` identifiers and lists `ftts` and `flowery-tts` in `/v4/info`, so lavalink-client's `source: "ftts"` searches work as on Lavalink with LavaSrc. No API key is needed; `WAVYZON_PLUGIN_FLOWERY_ENABLED=false` turns it off.

---

## 🎚️ Equalizer APO configurations

The new `apo` plugin, **on by default**, plays through **Equalizer APO** configs and presets with `filters.pluginFilters.apo`: parametric filters, GraphicEQ, convolution with impulse responses (zero added latency) and a reverb extension. Presets live in a directory and reload without a restart.

---

## 🧩 Plugin API

Plugins can now add **lyrics providers** (`Registry::lyrics`), asked before lyrics.ovh, on top of sources, filters, event listeners and routes. The change is additive: `API_VERSION` stays `1`.

---

## 🛠️ Fixes

- A stream that breaks mid-track is **reopened where it stopped** instead of ending the track.
- SoundCloud plays its **HLS AAC** streams, never the adaptive playlist.
- Mirroring no longer picks **unrelated uploads or remixes**, and a tone-like source is never used as a text-search fallback.
- A `noReplace` play never seeks the track a crossfade started.
- Per-attempt header timeouts, a server header-read timeout, and several Cadence parity gaps closed after a module-by-module audit.

---

## 🐳 Deployment and docs

- A **Pelican / Pterodactyl egg** (`contrib/pelican/`), built in pelican-eggs' stock images with no start script, exposing every setting as a panel variable.
- A documentation site (mdBook) at **wavyzon.ihorizon.org/docs/**, with a quick start, examples, a Pelican guide and the API reference.
- A new logo and brand assets.

---

Older and translated changelogs are archived in [`changelogs/`](changelogs/README.md). Version française : [`CHANGELOG_FR.md`](CHANGELOG_FR.md).
