# Genius lyrics

The `genius` plugin gets lyrics from [Genius](https://genius.com) for
`/v1/lyrics` and `/v4/lyrics` (LavaLyrics' API, see [REST](api/rest.md#lyrics)).
It is **on by default** (cargo feature `plugin-genius`) and last in the
default `WAVYZON_LYRICS_ORDER` (`lrclib,lyricsovh,genius`):

```text
LRCLIB ──has none / fails──► lyrics.ovh ──► Genius
```

Put it first (`WAVYZON_LYRICS_ORDER=genius,lrclib,lyricsovh`) or turn it
off. A Genius failure (timeout, refusal, unknown page layout) is logged,
counted in `wavyzon_lyrics_plugin_total{outcome="failed"}`, and the next
provider answers in its place, so clients never see it.

> **Known limit (November 2026)**: the search works with a key, but the
> lyrics live on genius.com song pages, which answered `403` to every server
> tried so far, keys or not. After a `403` or `429` from genius.com the
> plugin skips Genius for 10 minutes (the next provider answers at once),
> then tries again. This is why it comes last by default.

## Credentials

Genius' API searches songs but has no lyrics endpoint: the plugin searches,
then reads the lyrics from the song page on genius.com. How it searches
depends on what you give it:

| Setting | Search through |
| --- | --- |
| `WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN` | The official API (`api.genius.com/search`). **Recommended.** |
| `WAVYZON_PLUGIN_GENIUS_CLIENT_ID` + `_CLIENT_SECRET` | The official API, with a token exchanged at `POST /oauth/token` (`grant_type=client_credentials`). If Genius refuses the exchange, the plugin logs it, uses the public search, and retries 10 minutes later. |
| neither | genius.com's public web search. |

Get a token at <https://genius.com/api-clients>: create an API client, then
**Generate Access Token** (the *Client Access Token*). A token Genius
rejects (`401`) is logged, and that lookup uses the public search.

Without a key, expect Genius to refuse the public search from servers
(`403`, an anti-bot page). The token avoids that for the search; the song
page is still fetched from genius.com (see the limit above).

## Matching

The search is the track's cleaned title and first artist (the same cleaning
as lyrics.ovh: no `(feat. …)`, `- Remastered 2011`, `(Official Video)`,
` - Topic`, `VEVO`). The first hit is kept only if:

- its title equals the track's, both cleaned, ignoring case and punctuation
  (`Don’t` = `Don't`), or equals it once bracketed parts are dropped;
- its primary artist or its credited artists contain the track's first
  artist as whole words (`Queen` matches `Queen & David Bowie`);
- its lyrics are published (`lyrics_state` complete) and it is not marked
  instrumental.

Otherwise Genius "has none" and the next provider is asked. A wrong song's lyrics
are worse than another provider's, so the match is strict on purpose.

The lyrics are the page's `data-lyrics-container` blocks, `<br>` as line
breaks, without the header Genius puts inside them (contributors,
translations). Section tags like `[Chorus]` are kept.

## Settings

All optional, from `WAVYZON_PLUGIN_GENIUS_<KEY>`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `ENABLED` | `true` | `false` registers nothing: no Genius |
| `ACCESS_TOKEN` | none | Client Access Token |
| `CLIENT_ID`, `CLIENT_SECRET` | none | Exchanged for a token; both or neither (else the node stops at startup) |
| `TIMEOUT_MS` | `10000` | Bound of a whole Genius lookup (search + page); the next provider is asked after it |
| `PROXY` | none | `http://`, `https://` or `socks5://` proxy |
| `API_URL` | `https://api.genius.com` | API base, for a mirror or tests |
| `WEB_URL` | `https://genius.com` | Site base (public search, song pages) |

Leaving `genius` out of `WAVYZON_PLUGINS` does not load the plugin at all.
With LRCLIB and lyrics.ovh off, Genius alone serves lyrics.

## Caching and limits

Found lyrics are kept 7 days, misses 6 hours (20 000 songs each), keyed by
the cleaned artist and title; failures are not cached. Song pages are read
up to 4 MiB. Genius' secrets never appear in logs or errors.

## Checking it live

```bash
./target/release/livecheck -lyrics "Queen - Under Pressure" 248
```

(the optional last argument is the track length in seconds, which LRCLIB
matches) prints the provider order, which provider answered, and the first
lines
(with the node's environment, so set the same `WAVYZON_PLUGIN_GENIUS_*`).
