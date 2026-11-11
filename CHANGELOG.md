# Version 0.1.0 (first release — the Rust rewrite)

---

## 🦀 A Rust rewrite of Cadence

Wavyzon is a full rewrite of [Cadence](https://github.com/relateful/Cadence), the Go audio node, in **Rust**. It keeps what made Cadence good — **two-deck mixing** with real crossfades, the **frame-accurate wire clock** behind `TrackAudibleEvent` and synced lyrics, and **DAVE end-to-end encryption** for Discord voice — and stays a **drop-in Lavalink v4 node**: point your existing client at it and it plays.

---

## 🔐 DAVE without libdave

Voice encryption, including DAVE, is now **pure Rust**. There is no native library to download or link anymore, and libopus is built and linked statically: the result is a **single self-contained binary**.

---

## 🌐 A new HTTP audio source

Play **direct audio URLs** and **Icecast / Shoutcast radio streams** out of the box. Private and local network addresses are refused by default (`WAVYZON_HTTP_ALLOW_PRIVATE=false`), so a node exposed to clients cannot be turned against your own network.

---

## 🧩 A plugin API

The new `wavyzon-plugin` crate lets you add **sources, audio filters, event listeners and REST routes** without forking Wavyzon. Plugins are enabled with `WAVYZON_PLUGINS` and configured with `WAVYZON_PLUGIN_<NAME>_<KEY>` variables. An example plugin ships in `plugins/`.

---

## 🗣️ Flowery text-to-speech

The new `flowery` plugin, **on by default**, makes the bot speak with [Flowery](https://flowery.pw)'s free TTS. It reads LavaSrc's `ftts://<text>?voice=…&speed=…` identifiers and lists `ftts` and `flowery-tts` in `/v4/info`, so lavalink-client's `source: "ftts"` searches work as on Lavalink with LavaSrc. No API key is needed; `WAVYZON_PLUGIN_FLOWERY_ENABLED=false` turns it off.

---

## 📚 Better documentation

A new `docs/` folder covers configuration, the API, the plugin API and deployment, and the API reference is published from `cargo doc` with every release.

---

## 🔌 API

- The `/v1` API, the Lavalink v4 layer and the WebSocket events are Cadence's. New: `GET /v1/plugins`, plugin routes under `/v1/plugins/<name>`, `plugins` and `pluginFilters` in `/v1/info`, and `track.identifier` in player updates.
- v4 extra events are switched on with `wavyzonEvents` (Cadence's `cadenceEvents` still works).
- Logs are JSON by default; `WAVYZON_LOG_FORMAT=pretty` for terminals, and `RUST_LOG` for per-crate filters.
- `livecheck` checks search, mirroring, streaming and seeking against the live sources you configured.

---

## 🎵 Sources

- **SoundCloud** is the default playback source (`WAVYZON_PLAYBACK_SOURCES`); **HTTP** links and **local** files play directly.
- **Deezer, Spotify and Apple Music** are used for **metadata and search** only; matching tracks are played from a playback source. The Deezer streaming options (`DEEZER_ARL`, `DEEZER_MASTER_KEY`, ...) are gone.

---

## 🐳 Deployment

- A new multi-stage **Docker image** running as an unprivileged user, with a `docker-compose.yml` to get started.
- Every Cadence setting was renamed from `CADENCE_*` to `WAVYZON_*`. See `.env.example`.

---

## ⚖️ License

Wavyzon is released under **CC BY-NC-SA 4.0**, like the rest of the iHorizon projects. Cadence's MIT notice is kept in `NOTICE`.

---

Older and translated changelogs are archived in [`changelogs/`](changelogs/README.md). Version française : [`CHANGELOG_FR.md`](CHANGELOG_FR.md).
