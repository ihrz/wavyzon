# Common development tasks. `just` lists them. Same recipes as the Makefile.

image := "registry.gitlab.com/ihrz/wavyzon"
tag := "dev"

default:
    @just --list

# Format all crates
fmt:
    cargo fmt --all

# Check formatting without changing files
fmt-check:
    cargo fmt --all --check

# Run clippy on everything, warnings are errors
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the test suite
test *args:
    cargo test --workspace --all-features {{args}}

# Unit tests (in-crate #[cfg(test)] modules) and doctests
test-unit:
    cargo test --workspace --all-features --lib --bins
    cargo test --workspace --all-features --doc

# Mocked integration tests (fake voice gateway, mock sources, node e2e)
test-mock:
    cargo test --workspace --all-features --test '*'

# What CI runs: fmt-check, lint, test
check: fmt-check lint test

# Release build (target/release/wavyzon)
release:
    cargo build --release --locked -p wavyzon

# Run the node with settings from .env
run *args:
    cargo run -p wavyzon -- {{args}}

# Build the API docs and open them
doc:
    cargo doc --workspace --no-deps --all-features --open

# Check licences, advisories, bans and sources (needs cargo-deny)
deny:
    cargo deny check

# Build the Docker image
docker:
    docker build --build-arg VERSION={{tag}} -t {{image}}:{{tag}} .

# Run the Docker image with .env on port 2333
docker-run:
    docker run --rm -it --env-file .env -p 127.0.0.1:2333:2333 {{image}}:{{tag}}
