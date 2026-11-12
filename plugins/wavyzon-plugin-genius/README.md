# wavyzon-plugin-genius

[Genius](https://genius.com) lyrics for Wavyzon's `/v1/lyrics` and
`/v4/lyrics` (LavaLyrics' API). Genius is asked first; when it has nothing
or fails, the node's built-in lyrics.ovh client answers. With the plugin
off, lyrics.ovh answers directly.

On by default (cargo feature `plugin-genius`). Give it a Client Access Token
from <https://genius.com/api-clients> for reliable searches:

```bash
WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN=…
# or WAVYZON_PLUGIN_GENIUS_CLIENT_ID=… + WAVYZON_PLUGIN_GENIUS_CLIENT_SECRET=…
```

Without a key it uses genius.com's public search, which Genius often
refuses from servers (lyrics.ovh then answers). Turn it off with
`WAVYZON_PLUGIN_GENIUS_ENABLED=false`. Settings, matching rules and caching:
[Genius lyrics](../../docs/src/genius-lyrics.md).

| Module | |
| --- | --- |
| `lib.rs` | settings, authentication (token, client credentials, none), search, song page, cache, the plugin |
| `pick.rs` | which search hit is the track (strict title + artist match) |
| `html.rs` | lyrics out of the song page (`data-lyrics-container`), entities, tidying |
| `tests.rs` | against a fake Genius on a local port |
