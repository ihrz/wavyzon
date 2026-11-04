# Sources

A **source** turns a query or link into tracks, and, if it is *playable*,
opens an audio stream for them. Wavyzon has two kinds:

- **Playback sources** stream audio: SoundCloud, HTTP, local files, and
  plugin sources.
- **Metadata sources** only describe recordings: Deezer, Spotify and Apple
  Music. Their tracks are **mirrored** to a playback source when played.

| Source | `source` name | Search prefix | Links | Plays audio |
| --- | --- | --- | --- | --- |
| **SoundCloud** | `soundcloud` | `scsearch:` | tracks, sets, short links (`on.soundcloud.com`) | ✅ |
| **HTTP** | `http` | | any other `http(s)://` URL: files, Icecast/Shoutcast | ✅ |
| **Local files** | `local` | | `local:` paths under `WAVYZON_LOCAL_ROOT` | ✅ (opt-in) |
| **Deezer** | `deezer` | `dzsearch:`, `dzisrc:` | tracks, albums, playlists, artists, short links | mirrored |
| **Spotify** | `spotify` | `spsearch:` | tracks, albums, playlists, artists, `spotify:` URIs, short links | mirrored |
| **Apple Music** | `applemusic` | `amsearch:` | songs, albums, playlists, artists | mirrored |
| **Plugins** | plugin-defined | plugin-defined | plugin-defined | if the plugin implements `Playable` |

Plain-text queries (no prefix, not a URL) search the sources in
`WAVYZON_SEARCH_ORDER` (default `applemusic,spotify,deezer`) and return the
first non-empty result.

## SoundCloud

Full-length streams of public SoundCloud tracks. Wavyzon scrapes a client id
automatically (or uses `SOUNDCLOUD_CLIENT_ID`) and refreshes it when
SoundCloud rotates it. Progressive MP3 is preferred; HLS is used when that's
all a track offers. Go+ previews (30 s snippets) are rejected rather than
played as if they were the full track.

## HTTP

Any `http://` or `https://` URL that no other source claims is handled by the
HTTP source.

```http
GET /v1/loadtracks?identifier=https%3A%2F%2Fexample.com%2Fmusic%2Fsong.flac
```

**Files.** MP3, AAC (ADTS), M4A/MP4 (AAC), FLAC, Ogg Vorbis and WAV, decoded
by Symphonia. The format is detected from the `Content-Type`, the extension
and finally by probing the bytes. Title, artist, album and duration come from
the file's tags when present (ID3, Vorbis comments, MP4 atoms), otherwise
from the file name.

**Radio.** Icecast and Shoutcast streams are detected (`icy-*` headers, no
`Content-Length`, `audio/mpeg`/`audio/aac`/`audio/ogg` without an end) and
loaded as `isStream: true` with the station name (`icy-name`) as title.
Streams have no duration and can't be seeked.

**Seeking.** When the server supports `Range` requests (`Accept-Ranges:
bytes`), a seek fetches from the target offset instead of downloading from
the start. For formats where the byte offset of a timestamp can't be derived
exactly (VBR MP3 without a seek table, for example), Wavyzon falls back to
decoding from the start and skipping forward, so seeks are always
sample-accurate.

**Safety.** By default, the HTTP source **refuses private addresses**:
loopback (`127.0.0.0/8`, `::1`), RFC 1918 (`10/8`, `172.16/12`, `192.168/16`),
link-local (`169.254/16`, `fe80::/10`), unique-local (`fc00::/7`), CGNAT
(`100.64/10`) and unspecified addresses. The check is done on the resolved
address of every connection, including after redirects, so DNS tricks don't
get around it. Set `WAVYZON_HTTP_ALLOW_PRIVATE=true` only if every client that
can reach the node is trusted.

> Note: provisional. Playlist files (`.m3u`, `.pls`), the exact address
> list, redirect limits and timeouts of the HTTP source.

## Local files

Disabled unless `WAVYZON_LOCAL_ROOT` is set. Then:

```http
GET /v1/loadtracks?identifier=local%3Aalbums%2Fdiscovery%2F01-one-more-time.flac
```

plays `$WAVYZON_LOCAL_ROOT/albums/discovery/01-one-more-time.flac`. Paths are
always relative to the root; `..`, absolute paths and symlinks pointing
outside the root are refused. Same formats and tag reading as the HTTP
source; seeking is always cheap.

> Note: provisional. The `local:` identifier syntax and whether directories
> load as playlists.

## Metadata sources

### Deezer

Search (`dzsearch:`), ISRC lookup (`dzisrc:`), and links to tracks, albums,
playlists and artists (top tracks), through Deezer's public API. No account
needed.

Wavyzon **does not decrypt or stream Deezer audio**. Deezer tracks are
mirrored like Spotify and Apple Music ones.

### Spotify

Requires `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET` (create an app on the
Spotify developer dashboard). Playlists load up to 1 000 tracks. Spotify only
serves public, user-made playlists to third-party apps: its editorial and
personalised playlists (Today's Top Hits, Discover Weekly, Daily Mix) and
private playlists return `empty`.

### Apple Music

Works without configuration (a developer token is scraped from the web
player); set `APPLE_MUSIC_TOKEN` to use your own, and
`APPLE_MUSIC_STOREFRONT` for a non-US catalog.

## Mirroring

When a metadata-only track plays, Wavyzon finds the **same recording** on a
playback source:

```text
spotify / applemusic / deezer track
        │
        ▼
  has ISRC? ──yes──► look it up on each WAVYZON_PLAYBACK_SOURCES source (by ISRC)
        │                        │ found
        no / not found           ▼
        │                    play it
        ▼
  scored text search on each WAVYZON_PLAYBACK_SOURCES source
     "title artist", penalising remix / live / sped up / nightcore / cover …
     rejecting candidates whose length differs by more than 5 s
        │
        ▼
  best match ──► play it        no match ──► TrackExceptionEvent + TrackEndEvent{loadFailed}
```

- An **ISRC** identifies one recording, so an ISRC match is the exact song.
- Sources in `WAVYZON_PLAYBACK_SOURCES` are tried **in order**; plugin
  sources that implement [`IsrcLookup`](plugins.md#isrc-lookup-and-recommendations)
  can take part.
- Matches are cached (14 days) and concurrent lookups for the same track are
  de-duplicated, so a song is resolved once.
- The player state's `playable` field shows what a track was mirrored to.

## Instant starts

Like Spotify, Wavyzon keeps the first seconds of upcoming tracks ready:

- `loadtracks` preloads its first result, so "search, then play" starts
  from memory.
- `POST /v1/prefetch` with the next tracks of your queue preloads them too.
- On play, a preloaded track starts decoding immediately while the rest is
  fetched in the background.
- With `WAVYZON_AUDIO_CACHE_DIR`, repeat-played tracks are kept on disk and
  start (and seek) without any network request.

## Plugin sources

A plugin can add a source with a search prefix, links it claims, ISRC
lookups and recommendations. Plugin sources are consulted **before** the
built-in ones for links they match, so a plugin can take over a domain. The
example plugin adds `tone:`:

```http
GET /v1/loadtracks?identifier=tone:440:30
```

```json
{ "loadType": "search", "data": [ { "encoded": "eyJzb3…", "info": { "source": "tone", "identifier": "440:30", "title": "Sine 440 Hz", "author": "wavyzon-plugin-example", "durationMs": 30000, "uri": "tone://440:30" } } ] }
```

See [Writing plugins](plugins.md#2-sources).
