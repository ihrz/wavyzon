<!--
Thanks for taking the time to report this. Please don't include your
WAVYZON_PASSWORD, API credentials or any other secret.
For security problems, use the "Security" template and mark the issue confidential (see SECURITY.md).
-->

## Area

<!-- Keep one. -->
- Voice (joining, silence, disconnects, DAVE)
- Playback and mixing (crossfades, seeking, filters)
- Sources (SoundCloud, HTTP / Icecast, local files, Deezer / Spotify / Apple Music metadata)
- Timing (positions, TrackAudibleEvent, lyrics sync)
- API and sessions (REST, WebSocket, resume)
- Plugins (plugin API, a built-in plugin)
- Analysis (fingerprints, loudness)
- Build and deployment
- Not sure

## What happened?

<!-- What you saw, and what you expected instead. -->

## How can we reproduce it?

<!-- The smallest set of steps or requests that shows the problem. -->

1. Load a track with ...
2. ...

## Logs

<!-- Relevant Wavyzon log lines around the problem. Remove anything private first. -->

```text
```

## Environment

- Wavyzon version: <!-- `version` from GET /v4/info, or the commit you built from -->
- Run with: <!-- Docker image / native build (OS, arch) -->
- Client library: <!-- the Lavalink client your bot uses, with its version -->
- Plugins enabled: <!-- WAVYZON_PLUGINS, or "default" -->
- Voice region: <!-- for voice problems, the region in the "voice session ready" log line -->

/label ~bug ~"needs triage"
