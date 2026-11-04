# Pelican egg

`egg-wavyzon.json` runs Wavyzon on a [Pelican](https://pelican.dev) panel (it
also imports into Pterodactyl, which reads the same egg format). The panel
labels and descriptions are in French.

## Import

Admin → Eggs → Import → pick `egg-wavyzon.json`. Create a server with it,
fill in **WAVYZON_PASSWORD** (8+ characters, not `youshallnotpass`), and give
it a port: Wavyzon listens on `0.0.0.0:${SERVER_PORT}`.

## What the install does

The install runs in `rust:1-bookworm` (like the repository's `Dockerfile`):

1. installs CMake and Clang (libopus is built from source and linked
   statically; DAVE is pure Rust, so there is **no libdave and no libopus**
   at runtime);
2. clones `WAVYZON_REPO` at `WAVYZON_REF` (default branch **`dev`**). The
   GitLab repository is **private** for now, so set `WAVYZON_GIT_TOKEN` (an
   admin-only variable): a project or personal access token with the
   `read_repository` scope, used as `https://oauth2:<token>@gitlab.com/…`.
   Without it the clone fails with a message saying so;
3. `cargo build --release --locked -p wavyzon`, on the server volume because
   the install container's `/tmp` is a small tmpfs;
4. copies the binary, its shared libraries and the dynamic loader to the
   server, so it runs on either yolks image whatever its glibc;
5. writes `start.sh`, which sets the address from `SERVER_PORT`, prints which
   secrets are set (never their values) and starts the node.

A release build needs about **1.5 GB of RAM per job** and a few minutes.
`WAVYZON_BUILD_JOBS` (default 2) limits parallelism; lower it if the install is
killed for lack of memory. To update, change `WAVYZON_REF` if needed and
**reinstall** the server: the panel keeps the data directory, cache and
state file.

The panel marks the server as started on the `wavyzon listening` log line.
Stopping sends `^C`, which triggers the graceful shutdown: sessions are saved
to `WAVYZON_STATE_FILE` and resumed on the next start.

## Variables

Every Wavyzon setting the egg exposes maps 1:1 to the variables in
`.env.example` (see `docs/src/configuration.md`). Differences from the
Cadence egg: `CADENCE_*` became `WAVYZON_*`; the Deezer streaming variables
(`DEEZER_ARL`, `DEEZER_MASTER_KEY`, `DEEZER_HIGH_QUALITY`) and the
libdave/godave versions are gone; there are new variables for the playback
sources, the HTTP and local sources, plugins, the shared cache and the log
format (`pretty` by default, so the console stays readable).

## Regenerate

`egg-wavyzon.json` is generated from `install.sh` and the variable list in
`gen.py`:

```sh
python3 contrib/pelican/gen.py contrib/pelican/install.sh contrib/pelican/egg-wavyzon.json
```

Edit those two files, not the JSON.
