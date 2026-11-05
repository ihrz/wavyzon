# Installation

Wavyzon ships as a single binary, `wavyzon`. You can run it in Docker or
build it from source. Either way you need one thing: a password.

```bash
export WAVYZON_PASSWORD="$(openssl rand -hex 24)"
```

Wavyzon refuses to start without `WAVYZON_PASSWORD`, and refuses Lavalink's
well-known default `youshallnotpass`.

## Docker (recommended)

With the repository's `docker-compose.yml`:

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cp .env.example .env        # then set WAVYZON_PASSWORD at least
docker compose up -d
```

or with plain Docker:

```bash
docker build -t wavyzon .
docker run -d --name wavyzon --env-file .env \
  -p 127.0.0.1:2333:2333 -v wavyzon-data:/var/lib/wavyzon \
  --restart unless-stopped wavyzon
```

Check that it is up:

```bash
curl -s -H "Authorization: $WAVYZON_PASSWORD" http://localhost:2333/v1/info | jq
```

The image contains everything voice needs (Opus is linked statically, DAVE is
pure Rust), so there is nothing else to install. It runs as an unprivileged
`wavyzon` user under `tini`, with:

| Setting | Value in the image |
| --- | --- |
| `WAVYZON_ADDRESS` | `0.0.0.0:2333` (change the *published* port, not this) |
| `WAVYZON_DATA_DIR` | `/var/lib/wavyzon` (a volume) |
| Health check | TCP connect to `HEALTHCHECK_PORT` (`2333`), since every HTTP route needs the password |

The compose file publishes the port on `127.0.0.1` only, runs the container
read-only with all capabilities dropped, and has commented lines for the disk
cache (`WAVYZON_AUDIO_CACHE_DIR=/var/lib/wavyzon/cache`) and a read-only music
folder for local files (`WAVYZON_LOCAL_ROOT=/music`). Keep anything Wavyzon
writes (state file, cache) under `/var/lib/wavyzon`.

A prebuilt image is referenced as `registry.gitlab.com/ihrz/wavyzon:latest`.

> Note: provisional. Availability and tags of the prebuilt image.

## From source

### Requirements

| Requirement | Why |
| --- | --- |
| Rust **1.85+** (stable; `rust-toolchain.toml` selects it) | edition 2024 |
| A C compiler and **CMake** | building libopus statically (through `audiopus_sys`) |
| Linux, macOS or Windows | Linux is what production runs on; the others work for development |

There is no `libdave`, no `libopus` package and no `pkg-config` setup to do:
the repository's `.cargo/config.toml` makes `audiopus_sys` build and link Opus
statically, and DAVE comes from the pure-Rust `davey` crate.

> If you prefer your system's libopus, set `OPUS_STATIC=0` and make libopus
> discoverable through `pkg-config`.

### Build and run

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cargo build --release -p wavyzon
WAVYZON_PASSWORD=change-me ./target/release/wavyzon
```

The result is a self-contained binary you can copy to any machine with the
same OS and libc.

### With plugins

Plugins are Rust crates compiled into the binary and switched on by cargo
features (see [Writing plugins](plugins.md)). To build the bundled example
plugin and enable it:

```bash
cargo build --release -p wavyzon --features plugin-example
WAVYZON_PASSWORD=change-me WAVYZON_PLUGINS=example ./target/release/wavyzon
```

Each plugin is a `plugin-<name>` cargo feature of the `wavyzon` crate;
`plugin-example` and `plugin-apo` (Equalizer APO configurations, see
[Filters](filters.md#equalizer-apo-configurations-apo)) are on by default
(`--no-default-features` builds a node with no plugins).

### Running the tests

```bash
cargo test --workspace
```

## Next steps

- Tune the node with [Configuration](configuration.md).
- Connect a Lavalink client: [Lavalink v4 compatibility](lavalink.md).
- Put it behind TLS and monitor it: [Deployment](deployment.md).
