# Quick start

From nothing to a bot playing music, in four steps. Pick **one** way to run
the node in step 2.

## 1. Pick a password

Every request to Wavyzon carries a shared secret, like Lavalink's password:

```bash
openssl rand -hex 24
```

Wavyzon refuses to start without `WAVYZON_PASSWORD`, and refuses Lavalink's
well-known default `youshallnotpass`.

## 2. Run the node

### Option A: Docker (one machine, recommended)

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cp .env.example .env     # set WAVYZON_PASSWORD at least
docker compose up -d     # listens on 127.0.0.1:2333
```

The image has everything voice needs: Opus is linked statically and DAVE is
pure Rust.

### Option B: a Pelican or Pterodactyl panel

Import the egg, create a server, set the password and a port: the panel
builds and runs it. Follow [Deploy on Pelican](guides/pelican.md), with
pictures.

### Option C: from source

Needs Rust **1.85+**, a C compiler and **CMake** (libopus is built
statically). No `libdave`, no `libopus` package.

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cargo build --release -p wavyzon
WAVYZON_PASSWORD=change-me ./target/release/wavyzon
```

The node is ready when it logs `wavyzon listening`.

## 3. Check it answers

```bash
export WAVYZON_PASSWORD=change-me
curl -s -H "Authorization: $WAVYZON_PASSWORD" http://localhost:2333/v1/info
```

```json
{ "version": "0.1.0", "sources": ["deezer", "soundcloud", "spotify", "applemusic", "http"], "dave": true, "mixing": true, "wireClock": true, "…": "…" }
```

Search something:

```bash
curl -s -H "Authorization: $WAVYZON_PASSWORD" \
  "http://localhost:2333/v1/loadtracks?identifier=scsearch:midnight%20city" | jq '.data[0].info'
```

`401` means a wrong password; `curl: (7)` means the node isn't running or
listens elsewhere (`WAVYZON_ADDRESS`).

## 4. Connect your bot

Wavyzon speaks **Lavalink v4**, so any Lavalink client works: give it the
host, port and password. With
[`lavalink-client`](https://github.com/Tomato6966/lavalink-client):

```ts
import { Client, GatewayIntentBits } from "discord.js";
import { LavalinkManager } from "lavalink-client";

const client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates] });

client.lavalink = new LavalinkManager({
  nodes: [{ id: "wavyzon", host: "localhost", port: 2333, authorization: process.env.WAVYZON_PASSWORD! }],
  sendToShard: (guildId, payload) => client.guilds.cache.get(guildId)?.shard?.send(payload),
  client: { id: process.env.CLIENT_ID!, username: "my-bot" },
  playerOptions: { defaultSearchPlatform: "scsearch" }, // Wavyzon has no YouTube
});

client.on("raw", (d) => client.lavalink.sendRawData(d));
client.once("ready", () => client.lavalink.init({ id: client.user!.id, username: client.user!.username }));
```

Then play something from a command:

```ts
const player = client.lavalink.createPlayer({
  guildId: interaction.guildId!,
  voiceChannelId: member.voice.channelId!,
  textChannelId: interaction.channelId,
  selfDeaf: true,
});
await player.connect();
const res = await player.search({ query: "midnight city m83" }, interaction.user);
await player.queue.add(res.tracks[0]);
if (!player.playing) await player.play();
```

> **Tip** — use `scsearch:`, `dzsearch:`, `spsearch:` or `amsearch:` instead
> of `ytsearch:`; Spotify, Apple Music and Deezer results are matched by ISRC
> to a source that can play. Plain text searches follow
> `WAVYZON_SEARCH_ORDER`.

More ready-to-copy code (filters, crossfades, synced lyrics, Equalizer APO):
[Examples](examples.md). Other client libraries:
[Lavalink v4 compatibility](lavalink.md#client-setup).

## Next

- Tune it: [Configuration](configuration.md) (sources, Deezer, cache,
  crossfade length, plugins).
- Put it behind TLS and watch it: [Deployment](deployment.md),
  [Observability](observability.md).
- Something is off? [Troubleshooting & FAQ](troubleshooting.md).
