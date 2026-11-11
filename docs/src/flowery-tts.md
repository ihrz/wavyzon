# Flowery TTS

The `flowery` plugin speaks text with [Flowery](https://flowery.pw)'s free
text-to-speech API. It is **on by default** (cargo feature `plugin-flowery`)
and uses the identifiers of LavaSrc's Flowery source, so Lavalink clients
that know `ftts` work unchanged.

```text
GET /v4/loadtracks?identifier=ftts://Hello%20world%21
GET /v4/loadtracks?identifier=ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23&speed=1.2
```

With [lavalink-client](lavalink.md#lavalink-client-javascript--typescript):

```ts
const params = new URLSearchParams({ voice: "fa3ea565-121f-5efd-b4e9-59895c77df23" });
const res = await player.search(
  { query: `${encodeURIComponent(text)}?${params}`, source: "ftts" },
  requester,
);
player.queue.add(res.tracks[0]);
```

lavalink-client refuses an `ftts` search unless `/v4/info` lists `ftts`,
`flowery-tts` or `flowerytts` in `sourceManagers`: Wavyzon lists the first
two while the plugin is enabled.

## Identifiers

`ftts://<text>[?<options>]`, where `<text>` is percent-encoded (as
`encodeURIComponent` does). `ftts:<text>` and an upper-case `FTTS` are
accepted too. A `?` is only read as the start of the options when every
parameter after it is one of the options below, so an unencoded "How are
you?" keeps its question mark.

| Option | Values | Default |
| --- | --- | --- |
| `voice` | a voice id or name from [`/v1/tts/voices`](https://api.flowery.pw/v1/tts/voices) | `WAVYZON_PLUGIN_FLOWERY_VOICE`, else Flowery's default |
| `translate` | `true`/`false` (also `1/0`, `yes/no`): translate the text to the voice's language | `false` |
| `silence` | milliseconds of silence after the speech, clamped to 0–10 000 | `0` |
| `speed` | 0.5–10 (clamped) | `1.0` |
| `audio_format` | `mp3`, `ogg_vorbis`, `aac`, `wav`, `flac` | `mp3` |

`ogg_opus` and `pcm` (accepted by Flowery and LavaSrc) are played as `mp3`:
Wavyzon's decoder has no Opus decoder. Unknown parameters are ignored.

## Load results

Loading never calls Flowery; the track is built from the identifier:

| Field | Value |
| --- | --- |
| `sourceName` | `flowery-tts` (LavaSrc's name) |
| `identifier` | the canonical `ftts://` identifier (text and the options you gave) |
| `title` | the text, cut to 100 characters |
| `author` | `Flowery TTS` |
| `length` | `0` (unknown until synthesised) |
| `isStream` | `false` |
| `uri` | the Flowery request, `https://api.flowery.pw/v1/tts?text=…&voice=…` |

- No text (`ftts://`) → `loadType: "empty"`.
- More than 2 048 characters (Flowery's limit), an unreadable `speed` or
  `silence`, a `translate` that is not a boolean, an unknown `audio_format`
  or a voice over 128 characters → `loadType: "error"` (severity `common`).

Playing the track streams `GET /v1/tts` through the same HTTP client and
decoder as the [HTTP source](sources.md#http), so the audio cache and
instant starts work as for any track. Flowery answers once the whole text
is synthesised; a refused request (`429`) is a `suspicious` failure, other
errors `common`. Seeking makes the player skip forward from the start.

## Settings

All optional, as `WAVYZON_PLUGIN_FLOWERY_<KEY>`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `WAVYZON_PLUGIN_FLOWERY_ENABLED` | `true` | `false` registers no source (`ftts` disappears from `sourceManagers`) |
| `WAVYZON_PLUGIN_FLOWERY_VOICE` | Flowery's default | Default voice id or name |
| `WAVYZON_PLUGIN_FLOWERY_TRANSLATE` | `false` | Default `translate` |
| `WAVYZON_PLUGIN_FLOWERY_SILENCE` | `0` | Default `silence` (0–10 000 ms) |
| `WAVYZON_PLUGIN_FLOWERY_SPEED` | `1.0` | Default `speed` (0.5–10) |
| `WAVYZON_PLUGIN_FLOWERY_AUDIO_FORMAT` | `mp3` | Default `audio_format` |
| `WAVYZON_PLUGIN_FLOWERY_API_URL` | `https://api.flowery.pw/v1/tts` | Flowery's endpoint (a mirror) |
| `WAVYZON_PLUGIN_FLOWERY_TIMEOUT_MS` | `30000` | How long to wait for Flowery to start answering |
| `WAVYZON_PLUGIN_FLOWERY_PROXY` | | `http://`, `https://` or `socks5://` proxy |

An out-of-range value stops the node at startup with a message. Flowery
needs no API key.

## Turning it off

- `WAVYZON_PLUGIN_FLOWERY_ENABLED=false`: the plugin stays listed in
  `plugins` but adds no source.
- Or leave it out of [`WAVYZON_PLUGINS`](configuration.md#plugins)
  (`WAVYZON_PLUGINS=example,apo`): it is not loaded at all.
- Or build without it: `cargo build -p wavyzon --no-default-features
  --features plugin-example,plugin-apo`.
