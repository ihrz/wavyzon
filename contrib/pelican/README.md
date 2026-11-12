# Pelican / Pterodactyl egg

Two generated files run Wavyzon on a game-server panel:

| File | Panel |
| --- | --- |
| `egg-wavyzon.yaml` | [Pelican](https://pelican.dev) (PLCN_v3) |
| `egg-pterodactyl-wavyzon.json` | [Pterodactyl](https://pterodactyl.io) (PTDL_v2) |

The same files are submitted to
[pelican-eggs/software](https://github.com/pelican-eggs/software) under
`wavyzon/`, so the egg follows that repository's rules: stock images only
(`installers:debian` to build, `yolks:debian`/`yolks:ubuntu` to run) and no
start script.

## Import

Pelican: Admin → Eggs → Import → `egg-wavyzon.yaml`. Pterodactyl: Nests →
Import Egg → `egg-pterodactyl-wavyzon.json`. Create a server with it, fill in
**WAVYZON_PASSWORD** (8+ characters, not `youshallnotpass`) and give it a
port.

## What the install does

The install runs in `ghcr.io/pelican-eggs/installers:debian` (bookworm):

1. installs `build-essential` and CMake (libopus is built from source and
   linked statically; DAVE is pure Rust, so there is **no libdave and no
   libopus** at runtime), then a minimal stable Rust toolchain with rustup;
2. clones `WAVYZON_REPO` at `WAVYZON_REF` (default **`main`**; a
   release tag pins a version). The repository is public, so no token is
   needed. `WAVYZON_GIT_TOKEN` (admin-only) is only for a private fork: a
   project or personal access token with the `read_repository` scope, used as
   `https://oauth2:<token>@gitlab.com/…`. If the clone fails, the install stops
   with a message saying so;
3. `cargo build --release --locked -p wavyzon`, on the server volume because
   the install container's `/tmp` is a small tmpfs;
4. copies the binary to the server and deletes the toolchain and the build
   tree.

The binary only links glibc, libm and libgcc_s, which both runtime images
ship, with a newer glibc than the installer's. The start command is
`WAVYZON_ADDRESS=0.0.0.0:{{SERVER_PORT}} ./wavyzon`; the node creates its
data and cache directories itself.

A release build needs about **1.5 GB of RAM per job** and a few minutes.
`WAVYZON_BUILD_JOBS` (default 2) limits parallelism; lower it if the install is
killed for lack of memory. To update, change `WAVYZON_REF` if needed and
**reinstall** the server: the panel keeps the data directory, cache and
state file.

The panel marks the server as started on the `wavyzon listening` log line.
Stopping sends `^C`, which triggers the graceful shutdown: sessions are saved
to `WAVYZON_STATE_FILE` and resumed on the next start.

## Variables

Every setting the egg exposes maps 1:1 to the variables in `.env.example`
(see `docs/src/configuration.md`); `WAVYZON_REPO`, `WAVYZON_REF`,
`WAVYZON_GIT_TOKEN` and `WAVYZON_BUILD_JOBS` only drive the install.
Differences from the Cadence egg: `CADENCE_*` became `WAVYZON_*`; the
libdave/godave versions are gone; there are new variables for the playback
sources, the HTTP and local sources, plugins, the shared cache, crossfade,
lyrics (Genius, LRCLIB, lyrics.ovh and their order), Flowery text-to-speech and the log format (`pretty` by default, so the console stays
readable).

## Regenerate

Never edit the egg files by hand. Change `install.sh`, `icon.png` or the
variable list in `gen.py`, then:

```sh
python3 contrib/pelican/gen.py contrib/pelican
```

`gen.py` needs only the standard library. Before submitting an update
upstream, import the YAML into a panel, test an install, and export it from
the panel (pelican-eggs asks for panel exports).
