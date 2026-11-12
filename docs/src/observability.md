# Observability

Wavyzon is built so that **every dropped frame, stall and failure** shows up
three ways: as an **event** to the client, a **log line** and a **metric**.

## Metrics

`GET /metrics` serves the Prometheus text format. It requires the
`Authorization` header like every other route:

```yaml
# prometheus.yml
scrape_configs:
  - job_name: wavyzon
    metrics_path: /metrics
    authorization:
      type: ""            # send the raw password, no "Bearer " prefix
      credentials: change-me
    static_configs:
      - targets: ["wavyzon-eu:2333", "wavyzon-us:2333"]
```

> If your Prometheus version insists on a scheme, put a reverse proxy in
> front that adds `Authorization: <password>` for `/metrics`, or scrape from a
> sidecar.

Metric names follow Cadence's with the prefix `cadence_` → `wavyzon_`;
source counters come from `wavyzon-sources`.

### Gauges

| Metric | Meaning |
| --- | --- |
| `wavyzon_players` | Players held |
| `wavyzon_playing_players` | Players currently sending audio |
| `wavyzon_sessions` | Client sessions |
| `wavyzon_plugins` | Loaded plugins |
| `wavyzon_analysis_queued` | Offline analyses waiting |
| `wavyzon_fingerprints` | Track analyses held in memory |
| `wavyzon_audio_cache_bytes` | Bytes in the disk audio cache |
| `wavyzon_audio_cache_files` | Tracks in the disk audio cache |

### Counters

| Metric | Labels | Meaning |
| --- | --- | --- |
| `wavyzon_events_total` | `type` | Player events emitted, by event type |
| `wavyzon_track_exceptions_total` | `kind` (`source:severity`) | Track failures |
| `wavyzon_voice_closes_total` | `code` | Voice gateway closes by close code |
| `wavyzon_send_stall_ms_total` | | Milliseconds the send loop fell behind its 20 ms schedule |
| `wavyzon_prefetch_total` | `outcome` = `queued`, `hit`, `miss`, `failed`, `dropped` | Track-start prefetch |
| `wavyzon_audio_cache_total` | `outcome` = `hit`, `miss`, `stored`, `evicted`, `read_failed`, `write_failed`, `incomplete` | Disk cache outcomes |
| `wavyzon_fallback_total` | `kind` = `search`, `mirror` | Answers given by a playback source other than the first one |
| `wavyzon_mirror_total` | `outcome` = `cached`, `isrc`, `search`, `miss`, `failed` | How metadata-only tracks were mirrored |
| `wavyzon_recommendations_total` | `outcome` = `served`, `cached`, `empty`, `failed` | Recommendation (radio) outcomes |
| `wavyzon_deezer_recommendations_total` | `source` = `track_mix`, `artist_radio` | Where Deezer's recommendation lists came from |
| `wavyzon_lyrics_total` | `outcome` = `found`, `cached`, `not_found`, `failed` | Lyrics lookups (lyrics.ovh) |
| `wavyzon_lyrics_plugin_total` | `outcome` = `found`, `not_found`, `failed` | Lookups by plugin lyrics providers (Genius), asked before lyrics.ovh; `not_found` and `failed` fall through to lyrics.ovh |
| `wavyzon_deezer_accounts_total` | `outcome` = `switched`, `rejected` | Deezer accounts rejected, and streams served by a later account |
| `wavyzon_seek_total` | `outcome` = `range`, `fallback` | How Deezer seeks were served (a byte range, or the whole file when the layout can't be seeked) |
| `wavyzon_cdn_range_ignored_total` | | Range requests a CDN answered with the whole file |
| `wavyzon_host_total` | `outcome` = `started`, `ended`, `refused` | DJ voice-overs |
| `wavyzon_peer_cache_total` | `outcome` = `index_hit`, `pulled`, `failed`, `too_busy`, `gossip_failed` | Cross-node cache outcomes |
| `wavyzon_analysis_dropped_total` | | Analyses dropped because the queue was full |
| `wavyzon_analysis_done_total` | | Tracks analysed offline |
| `wavyzon_analysis_failed_total` | | Offline analyses that failed |

Labelled counters only list label values seen so far, except the source
counters, which always list every outcome.

### Process

| Metric | Meaning |
| --- | --- |
| `wavyzon_tasks` | Live tokio tasks |
| `wavyzon_resident_bytes` | Resident memory (Linux; `0` elsewhere) |

### Useful alerts

```yaml
groups:
  - name: wavyzon
    rules:
      - alert: WavyzonSendStalls
        expr: rate(wavyzon_send_stall_ms_total[5m]) > 20     # >2% of real time lost
        for: 10m
        annotations: { summary: "Send loop is late on {{ $labels.instance }}: CPU starved?" }
      - alert: WavyzonSourceRefusing
        expr: sum by (kind) (rate(wavyzon_track_exceptions_total{kind=~".*:suspicious"}[10m])) > 0.1
        for: 10m
        annotations: { summary: "A source is refusing requests ({{ $labels.kind }}): rate limit or block" }
      - alert: WavyzonVoiceCloses
        expr: sum(rate(wavyzon_voice_closes_total{code!~"1000|4014"}[10m])) > 0.05
        for: 15m
        annotations: { summary: "Abnormal voice gateway closes" }
```

## Logs

Logs go to stdout through [`tracing`](https://docs.rs/tracing), one JSON
object per line by default:

```json
{"timestamp":"2026-11-03T10:15:02.511Z","level":"INFO","target":"wavyzon::api","fields":{"message":"client connected","session":"8e1f0c2a9b7d4e61","client":"my-bot/1.0","resumed":false}}
{"timestamp":"2026-11-03T10:15:04.020Z","level":"WARN","target":"wavyzon_sources::soundcloud","fields":{"message":"client id expired, refreshing"}}
```

- Level: `WAVYZON_LOG_LEVEL` (`info` by default), or a full `RUST_LOG`
  filter for per-crate control, e.g. `RUST_LOG=info,wavyzon_voice=debug`.
- Every log line about a player carries `guild` (and `session` where it
  applies), so you can filter one guild's story.
- Plugins log through the same subscriber (use `tracing` in your plugin);
  their target is the plugin crate name.

`WAVYZON_LOG_FORMAT=pretty` switches to human-readable lines for terminals.

### What to look for

| Log | Means |
| --- | --- |
| `resume backlog is full` | A client stayed away too long while resuming; it will miss messages |
| `voice closed` with a code | See [Troubleshooting](troubleshooting.md#voice) |
| `dave hold` / `restarting voice after DAVE hold` | E2EE epoch took > 15 s; the session was restarted |
| `proxy failed, using direct` | A per-source proxy is down (60 s direct fallback) |
| `send stall` | The host is overloaded or the process was descheduled |

## Events as telemetry

Events are also the cheapest way to measure user-visible quality from the
bot side:

- **Time to audio**: `TrackAudibleEvent.sentAt` minus the time you sent the
  player update.
- **Failure rate**: `TrackExceptionEvent` by `exception.source` and
  `exception.severity`.
- **Stalls**: `SendStallEvent.stallMs`, `TrackStuckEvent`.

An [event listener plugin](plugins.md#4-event-listeners) can export these
centrally for all your bots.
