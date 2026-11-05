# Deployment

## Sizing

Wavyzon is light: no JVM, no GC. Rough numbers per **playing** player:

| Resource | Cost |
| --- | --- |
| CPU | Opus encoding dominates: a few % of one core per player at 96 kbps, more with time-stretch, reverb or a crossfade (two decks) |
| Memory | ~2 MB per deck at the default `WAVYZON_BUFFER_MS=10000` (48 kHz × 2 ch × 2 B × 10 s), plus caches |
| Network out | The Opus bitrate (96 kbps default) + RTP overhead, per player |
| Network in | The source bitrate (128–320 kbps for files, more for FLAC) while buffering |

A 2-vCPU VM comfortably runs a few hundred concurrent players. Measure
`wavyzon_send_stall_ms_total` under your real load before trusting any
number.

## Regions

Latency from the node to Discord's voice servers matters more than latency
to your bot. Run one node per region (for example EU and US), and route
each guild to the node nearest its voice region:

- `playerUpdate.state.region` tells you the region of the current voice
  server (`rotterdam`, `us-east`, `brazil`…);
- move a guild by destroying the player on one node and creating it on the
  other with the same encoded track and `position`: encoded tracks are
  portable between nodes.

With `WAVYZON_CACHE_PEERS`, nodes can also share their disk audio cache.

## Behind a reverse proxy (TLS)

Expose Wavyzon over TLS if your bot and node don't share a private network.
The WebSocket needs the upgrade headers and a long read timeout.

**Caddy**

```caddyfile
wavyzon.example.com {
  reverse_proxy 127.0.0.1:2333
}
```

**nginx**

```nginx
server {
  listen 443 ssl http2;
  server_name wavyzon.example.com;
  # ssl_certificate …; ssl_certificate_key …;

  location / {
    proxy_pass http://127.0.0.1:2333;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_read_timeout 1h;
    proxy_send_timeout 1h;
  }
}
```

Then use `secure: true` / `wss://` / `https://` in your client, and set
`WAVYZON_ADDRESS=127.0.0.1:2333` so the node is reachable only through the
proxy.

Voice traffic (UDP to Discord) goes **out** directly from the node; it never
passes through the proxy and needs no inbound port.

## Pelican / Pterodactyl

[`contrib/pelican/`](https://gitlab.com/ihrz/wavyzon/-/tree/main/contrib/pelican)
has a panel egg (`egg-wavyzon.yaml` for Pelican,
`egg-pterodactyl-wavyzon.json` for Pterodactyl) that builds Wavyzon from
source at install time with the stock pelican-eggs images and exposes the
settings as panel variables. Import it, set `WAVYZON_PASSWORD` and give
the server a port; [its README](https://gitlab.com/ihrz/wavyzon/-/blob/main/contrib/pelican/README.md)
covers memory needs during the build and how to update.

## systemd

```ini
# /etc/systemd/system/wavyzon.service
[Unit]
Description=Wavyzon audio node
After=network-online.target
Wants=network-online.target

[Service]
User=wavyzon
EnvironmentFile=/etc/wavyzon/wavyzon.env
ExecStart=/usr/local/bin/wavyzon
Restart=on-failure
RestartSec=2
# graceful shutdown saves state (WAVYZON_STATE_FILE)
KillSignal=SIGTERM
TimeoutStopSec=20

# hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
StateDirectory=wavyzon
ReadWritePaths=/var/lib/wavyzon
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```bash
# /etc/wavyzon/wavyzon.env  (chmod 600)
WAVYZON_PASSWORD=...
WAVYZON_ADDRESS=127.0.0.1:2333
WAVYZON_DATA_DIR=/var/lib/wavyzon
WAVYZON_STATE_FILE=/var/lib/wavyzon/state.json
WAVYZON_AUDIO_CACHE_DIR=/var/lib/wavyzon/cache
```

## Zero-downtime-ish restarts

1. Enable resuming in your bot (`PATCH /v1/sessions/{id} {"resuming": true}`).
2. Set `WAVYZON_STATE_FILE`.
3. On `SIGTERM`, Wavyzon saves sessions and players and exits; on start it
   restores them, and your bot reconnects with its `Session-Id`. Playback
   continues from where it stopped (a short gap while the node restarts and
   voice reconnects).

## Production checklist

- [ ] Long random `WAVYZON_PASSWORD`, kept out of the image and the repository.
- [ ] Node not exposed to the internet without TLS; ideally bound to localhost or a private network.
- [ ] `WAVYZON_HTTP_ALLOW_PRIVATE=false` (the default).
- [ ] `WAVYZON_LOCAL_ROOT` only if you need local files, pointing at a read-only directory.
- [ ] Only trusted plugins compiled in and enabled.
- [ ] `/metrics` scraped and the alerts from [Observability](observability.md) set up.
- [ ] `WAVYZON_STATE_FILE` + client resuming for painless deploys.
- [ ] Disk cache on a volume with room for `WAVYZON_AUDIO_CACHE_GB`.
- [ ] Source credentials (Spotify) set; proxies configured if your IP gets rate-limited.
- [ ] `WAVYZON_PLAYOUT_DELAY_MS` measured if you show synced lyrics.
