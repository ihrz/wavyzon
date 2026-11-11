# wavyzon-plugin-flowery

[Flowery](https://flowery.pw) text-to-speech as a Wavyzon source, with the
identifiers of LavaSrc's Flowery source, so a Lavalink client's `ftts`
search works unchanged (lavalink-client checks that `/v4/info` lists `ftts`
or `flowery-tts` in `sourceManagers`; this plugin registers both).

```text
GET /v4/loadtracks?identifier=ftts://Hello%20world%21
GET /v4/loadtracks?identifier=ftts://Hello%20world%21?voice=<id or name>&translate=false&silence=500&speed=1.2&audio_format=mp3
```

On by default (cargo feature `plugin-flowery`), no API key. Turn it off with
`WAVYZON_PLUGIN_FLOWERY_ENABLED=false` or by leaving `flowery` out of
`WAVYZON_PLUGINS`. Options, bounds, settings and load results:
[Flowery TTS](../../docs/src/flowery-tts.md).

| Module | |
| --- | --- |
| `spec.rs` | `ftts://` parsing and bounds, the canonical identifier, the Flowery request URL |
| `lib.rs` | settings, the `flowery-tts`/`ftts` source (load + stream), the plugin |
