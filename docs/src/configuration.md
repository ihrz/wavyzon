# Configuration

Wavyzon is configured entirely through **environment variables**. There is no
config file; `.env.example` in the repository lists every variable with a
comment, and Docker's `--env-file` reads it directly.

Conventions used below:

- **Booleans** accept `1/true/yes/on` and `0/false/no/off` (case-insensitive).
  Anything else falls back to the default.
- **Lists** are comma-separated; spaces around items are ignored.
- **Numbers** out of range are **clamped** to the documented range, not
  rejected. A value that isn't a number at all stops the node at startup with
  an error naming the variable.
- Empty values count as unset.

## Core

| Variable | Default | Description |
| --- | --- | --- |
| `WAVYZON_PASSWORD` | **required** | Shared secret, sent by clients in the `Authorization` header of every request and of the WebSocket upgrade. Compared in constant time. The Lavalink default `youshallnotpass` is refused. |
| `WAVYZON_ADDRESS` | `0.0.0.0:2333` | Listen address (`host:port`). Use `127.0.0.1:2333` to accept local connections only. |
| `WAVYZON_DATA_DIR` | `./data` | Writable directory for plugin data. Created at startup if missing. Plugins each get `$WAVYZON_DATA_DIR/plugins/<name>` (created before they register). The Docker image sets it to `/var/lib/wavyzon`. |
| `WAVYZON_ALLOW_NO_VOICE` | `false` | Start even when this build cannot do voice (no Opus encoder or no DAVE). Without it such a build refuses to start, as Cadence did. Search, analysis and the API keep working. |

## Playback and timing

These are Cadence's settings with the `CADENCE_` prefix renamed to
`WAVYZON_`.

| Variable | Default | Range | Description |
| --- | --- | --- | --- |
| `WAVYZON_BUFFER_MS` | `10000` | 1 000 – 60 000 | Decoded audio buffered ahead, per deck. More smooths out slow sources at the cost of memory (about 192 KB per second per deck). |
| `WAVYZON_BITRATE` | `96000` | 16 000 – 384 000 | Opus bitrate used when the bot doesn't send one (`voice.bitrate` or `bitrate` in a player update). |
| `WAVYZON_PLAYOUT_DELAY_MS` | `0` | ≥ 0 | Measured delay between Discord receiving audio and listeners hearing it. Added to every `audibleAt`. See [the wire clock](wire-clock.md#playout-delay). |
| `WAVYZON_PAUSE_FADE_MS` | `0` | 0 – 2 000 | Fade out on pause and back in on resume. `0` pauses instantly. |
| `WAVYZON_CROSSFADE_MS` | `3000` | 0 – 12 000 | Automatic crossfade into the `nextTrack`: it fades in this long before the current track ends. `0` keeps strict gapless. See [mixing](mixing.md#automatic-crossfade). |
| `WAVYZON_UPDATE_INTERVAL_MS` | `1000` | ≥ 250 | How often `playerUpdate` is sent while a player is playing. An update is also sent right after every event. |
| `WAVYZON_RESUME_TIMEOUT_SECONDS` | `300` | > 0 | How long players survive a disconnected client when resuming is enabled. Clients can override it per session. |
| `WAVYZON_NORMALIZE` | `true` | | Level every track to the same loudness, using its analysis. |
| `WAVYZON_NORMALIZE_LUFS` | `-14` | −30 – −5 | Loudness target for levelling, in LUFS. |
| `WAVYZON_STATE_FILE` | off | | Where to save sessions and players on graceful shutdown, so a restart resumes playback where it stopped. |

## Sources

| Variable | Default | Description |
| --- | --- | --- |
| `WAVYZON_PLAYBACK_SOURCES` | `deezer,soundcloud` | Sources that metadata-only tracks (Spotify, Apple Music) are **mirrored** to, in order of preference. See [Sources](sources.md#mirroring). A mirror target must be able to play **and** search (or look up ISRCs), so `http` and `local` don't qualify: they only play their own links. Playable plugin sources that are not listed are tried after the listed ones. |
| `WAVYZON_LOCAL_ROOT` | off | Directory that the `local` source may read. Unset disables local files entirely. Paths can never escape this root. |
| `WAVYZON_SEARCH_ORDER` | `deezer,applemusic,spotify,soundcloud` | What a plain-text `loadtracks` query (no prefix) searches, in order. The first source with results wins. |
| `WAVYZON_PREFETCH_ON_LOAD` | `true` | Preload the first result of every `loadtracks`, so "search, then play" starts from memory. |
| `WAVYZON_HTTP_SOURCE` | `true` | Enable the HTTP source (direct audio URLs and internet radio). |
| `WAVYZON_HTTP_ALLOW_PRIVATE` | `false` | Allow the HTTP source to fetch private, loopback, link-local and other non-public addresses. Checked on every redirect and after DNS resolution. **Leave it off** on a node that untrusted clients can reach. |
| `WAVYZON_HTTP_MAX_REDIRECTS` | `5` | Redirects the HTTP source follows. |
| `WAVYZON_HTTP_TIMEOUT_MS` | `10000` | HTTP source connect/response timeout (minimum 500). |
| `WAVYZON_HTTP_PROXY` | | Proxy for the HTTP source. |
| `SOUNDCLOUD_ENABLED` | `true` | Enable SoundCloud (search, links and playback). |
| `DEEZER_ENABLED`, `SPOTIFY_ENABLED`, `APPLE_MUSIC_ENABLED` | `true` | Enable each source. |
| `DEEZER_ARL` | | Deezer account ARL cookies (the `arl` cookie of a logged-in account), comma-separated. Needed for playback: without one Deezer is metadata only and its tracks are mirrored like Spotify's. Several ARLs are rotated. |
| `DEEZER_ARL_ROTATION` | `failover` | How several ARLs are used: `failover` (always the first healthy account), `round-robin` or `random`. An unknown value is refused at startup. A rejected ARL is benched for 15 minutes. |
| `DEEZER_MASTER_KEY` | | The 16-byte Blowfish master key of the `BF_CBC_STRIPE` stream cipher. Required for playback; Wavyzon does not ship one. |
| `DEEZER_HIGH_QUALITY` | `true` | Ask for 128 kbit/s MP3 (falling back to 64) instead of 64 kbit/s only. |
| `SOUNDCLOUD_CLIENT_ID` | scraped | Optional fixed SoundCloud client id. When unset, Wavyzon scrapes one and refreshes it when it expires. |
| `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET` | | Spotify Web API credentials (client-credentials flow). Without them Spotify links and `spsearch:` don't resolve. Metadata only. |
| `APPLE_MUSIC_TOKEN` | scraped | Optional fixed Apple Music developer token. Metadata only. |
| `APPLE_MUSIC_STOREFRONT` | `us` | Apple Music catalog storefront (country code). |
| `DEEZER_PROXY`, `SPOTIFY_PROXY`, `APPLE_MUSIC_PROXY`, `SOUNDCLOUD_PROXY` | | Per-source HTTP(S) or SOCKS proxy. A proxy that fails falls back to a direct connection for 60 s. |

Deezer search, links and ISRC lookups work with no credentials (public
catalog). **Streaming** needs `DEEZER_ARL` (an account) and
`DEEZER_MASTER_KEY` (the stream cipher key): without them Deezer tracks are
mirrored to another playback source and a warning is logged at startup.

## Caching

| Variable | Default | Range | Description |
| --- | --- | --- | --- |
| `WAVYZON_AUDIO_CACHE_DIR` | off | | Directory for the disk audio cache. Unset disables it. |
| `WAVYZON_AUDIO_CACHE_GB` | `20` | 1 – 1 000 | Disk cache size cap. Least recently played tracks are evicted first. |
| `WAVYZON_CACHE_PEERS` | | | Other Wavyzon nodes (base URLs) sharing their disk cache. Every 30 s the node reads each peer's `/v1/cache/index`; when a track is opened that a peer has and this node doesn't, it is pulled in the background (at most 3 at a time), so later plays come from disk. The current play is never delayed. Needs `WAVYZON_CACHE_SECRET` and `WAVYZON_AUDIO_CACHE_DIR`. |
| `WAVYZON_CACHE_SECRET` | | | Secret peers use to read this node's cache (`/v1/cache/*`). Accepted *only* on the cache routes, so peers never get your main password. |

## Track analysis

| Variable | Default | Description |
| --- | --- | --- |
| `WAVYZON_ANALYSIS_WORKERS` | `2` | Offline analysis concurrency (BPM, key, sections, loudness…). |
| `WAVYZON_ANALYSIS_QUEUE` | `32` | Bound of the offline analysis queue. When full, new requests are dropped and counted. |

## Logging

| Variable | Default | Description |
| --- | --- | --- |
| `WAVYZON_LOG_LEVEL` | `info` | `debug`, `info`, `warn` or `error`. |
| `RUST_LOG` | | Standard `tracing` filter, e.g. `wavyzon_voice=debug,info`. Overrides `WAVYZON_LOG_LEVEL` when set. |
| `WAVYZON_LOG_FORMAT` | `json` | `json` (one object per line, for log collectors) or `pretty` (for terminals). Any other value stops the node at startup. |

## Plugins

| Variable | Default | Description |
| --- | --- | --- |
| `WAVYZON_PLUGINS` | all compiled in | Comma-separated plugin names to enable, in registration order (`example,lyrics`). **Empty enables every plugin compiled into the binary** (the default build compiles in `example`, cargo feature `plugin-example`). Naming a plugin that isn't compiled in stops the node at startup, and so does a plugin built for another `API_VERSION`. |
| `WAVYZON_PLUGIN_<NAME>_<KEY>` | | Settings for one plugin. `<NAME>` is the plugin name upper-cased with `-` turned into `_`; the plugin receives `<key>` lower-cased. `WAVYZON_PLUGIN_EXAMPLE_AMPLITUDE=0.5` → `ctx.setting("amplitude") == Some("0.5")`. |


## Example `.env`

```bash
# required
WAVYZON_PASSWORD=8c1e0f6b5f0d4a9c9b3e7c51f2a0d6e4

# network
WAVYZON_ADDRESS=0.0.0.0:2333

# sources
SPOTIFY_CLIENT_ID=...
SPOTIFY_CLIENT_SECRET=...
WAVYZON_PLAYBACK_SOURCES=soundcloud
WAVYZON_LOCAL_ROOT=/srv/music

# quality of life
WAVYZON_PAUSE_FADE_MS=250
WAVYZON_STATE_FILE=/var/lib/wavyzon/state.json
WAVYZON_AUDIO_CACHE_DIR=/var/lib/wavyzon/cache

# plugins
WAVYZON_PLUGINS=example
WAVYZON_PLUGIN_EXAMPLE_AMPLITUDE=0.3
```

## Startup warnings

Wavyzon logs a warning at startup, but still starts, for configuration that
is valid but probably not what you want, for example:

- Spotify credentials missing (Spotify links won't resolve);
- SoundCloud disabled with no other playback source configured;
- `WAVYZON_CACHE_PEERS` set without `WAVYZON_CACHE_SECRET` or
  `WAVYZON_AUDIO_CACHE_DIR` (the peer cache stays off);
- `WAVYZON_HTTP_ALLOW_PRIVATE=true` while `WAVYZON_ADDRESS` is not a
  loopback address;
- a name in `WAVYZON_PLAYBACK_SOURCES` that can't play or search (ignored).

It refuses to start (exit code 1, the reason logged) when the password is
missing or `youshallnotpass`, a number isn't a number, `WAVYZON_PLUGINS`
names an unknown plugin, a plugin fails to register, `WAVYZON_LOCAL_ROOT` or
`WAVYZON_DATA_DIR` is unusable, the address can't be bound, or the build has
no voice support and `WAVYZON_ALLOW_NO_VOICE` isn't set.
