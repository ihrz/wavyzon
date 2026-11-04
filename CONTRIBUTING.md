# Contributing to Wavyzon

Thanks for wanting to help! Wavyzon is developed on **GitLab** at
<https://gitlab.com/ihrz/wavyzon> and mirrored read-only to
[GitHub](https://github.com/ihrz/wavyzon). Issues and pull requests are
welcome on both, but merge requests on GitLab are the ones that get merged;
a GitHub pull request is re-applied on GitLab by a maintainer.

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
Security problems go through [SECURITY.md](SECURITY.md), never a public issue.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on the approach.
- Changes to behaviour that Lavalink v4 clients rely on (REST, WebSocket, events, track encoding) need a good reason and a note in the changelog. Compatibility is a feature.

## Setting up

You need:

- Rust **stable** (the repository pins it in `rust-toolchain.toml`; `rustup` picks it up). The minimum supported version is the `rust-version` in `Cargo.toml`.
- **CMake** and a **C compiler** (`build-essential` / `gcc` / `clang`). The build compiles libopus from source and links it statically (see `.cargo/config.toml`). To use a system libopus instead, set `OPUS_STATIC=0` and make it findable by `pkg-config`.
- Nothing else for voice: DAVE end-to-end encryption is pure Rust (the `davey` crate), so there is no libdave to install.

```sh
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cp .env.example .env    # set WAVYZON_PASSWORD at least
make run                # or: cargo run -p wavyzon
```

`make help` lists the other targets (`fmt`, `lint`, `test`, `doc`, `deny`, `docker`, ...). If you use [just](https://github.com/casey/just), the `justfile` has the same recipes.

## Before you open a merge request

```sh
make check   # cargo fmt --check, clippy -D warnings, tests
make test-unit   # just the unit tests (+ doctests)
make test-mock   # just the mocked integration tests (fake voice gateway, mock sources, node e2e)
make deny    # licences and advisories (needs cargo-deny)
```

CI runs the same checks on every merge request, plus a release build and `cargo-deny`.

- Format with `cargo fmt` (configured in `rustfmt.toml`, lines up to 120 columns).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` must be clean. If a lint is genuinely wrong for a spot, `#[allow(...)]` it locally with a short comment saying why.
- Add tests for new behaviour. Timing, mixing and protocol code should have deterministic tests; `crates/wavyzon-testkit` has helpers for that.
- Update the docs in `docs/`, the `README.md` and `.env.example` when you change configuration or the API.
- Describe user-visible changes in the merge request description; maintainers turn them into the changelog at release time.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, for example `fix(voice): reconnect after 4006` or
`feat(sources): follow Icecast metadata`. Common types: `feat`, `fix`,
`perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`. Scopes usually
match a crate or area: `voice`, `mixer`, `player`, `sources`, `http`, `api`,
`plugin`, `analysis`, `docs`, `ci`.

Mark breaking changes with `!` (`feat(api)!: ...`) and explain them in the body.

## Plugins

Plugins built on the `wavyzon-plugin` crate don't need to live in this
repository. Publish them on your own; open an issue if the plugin API is
missing something you need. Example plugins in `plugins/` must stay small and
build with the rest of the workspace.

## Releases

Maintainers release by bumping `version` in the root `Cargo.toml`, writing the
release notes in `CHANGELOG.md` and `CHANGELOG_FR.md`, archiving them
under `changelogs/en/<version>/` and `changelogs/fr/<version>/`, then pushing a
`v<version>` tag. The tag pipeline builds and publishes the Docker image.

## Licence of contributions

Wavyzon is licensed under [CC BY-NC-SA 4.0](LICENSE). By submitting a
contribution you agree that it is licensed under the same terms, and you
confirm that you have the right to submit it. Code copied from elsewhere must
come under a licence that allows that, and must keep its notice (see
[NOTICE](NOTICE)).
