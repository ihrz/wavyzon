#!/bin/bash
# Wavyzon - build from source in the stock installer image (ghcr.io/pelican-eggs/installers:debian).
# CMake and a C compiler build libopus, which is linked statically; DAVE is pure Rust. The binary only
# needs glibc, libm and libgcc_s at runtime, which the yolks debian/ubuntu images provide (their glibc
# is newer than the installer's bookworm one).
set -e
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends build-essential cmake

# /tmp is a small tmpfs in the Wings install container: build on the server volume instead.
BUILD=/mnt/server/.build
if [ -d "${BUILD}" ]; then chmod -R u+w "${BUILD}"; rm -rf "${BUILD}"; fi
mkdir -p "${BUILD}/tmp"
export TMPDIR="${BUILD}/tmp"
export RUSTUP_HOME="${BUILD}/rustup"
export CARGO_HOME="${BUILD}/cargo"
export CARGO_TARGET_DIR="${BUILD}/target"
export CARGO_INCREMENTAL=0
# Fewer parallel jobs = less RAM. A release build needs ~1.5 GB per job.
export CARGO_BUILD_JOBS="${WAVYZON_BUILD_JOBS:-2}"
export PATH="${CARGO_HOME}/bin:${PATH}"

echo "Installing the Rust toolchain..."
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
cargo --version

REPO="${WAVYZON_REPO}"
if [ -n "${WAVYZON_GIT_TOKEN}" ]; then
    # Private fork: https://oauth2:<token>@gitlab.com/...
    REPO="$(printf '%s' "${REPO}" | sed -E "s#^https://#https://oauth2:${WAVYZON_GIT_TOKEN}@#")"
fi
echo "Cloning ${WAVYZON_REPO} (${WAVYZON_REF})..."
if ! GIT_TERMINAL_PROMPT=0 git clone --depth 1 --branch "${WAVYZON_REF}" "${REPO}" "${BUILD}/src"; then
    echo "ERROR: could not clone ${WAVYZON_REPO} at ${WAVYZON_REF}."
    echo "For a private fork, set WAVYZON_GIT_TOKEN (GitLab access token with read_repository)."
    exit 1
fi
cd "${BUILD}/src"
echo "Commit: $(git rev-parse --short HEAD)"

# Build with the stable toolchain installed above instead of adding the components that
# rust-toolchain.toml asks for (rustfmt, clippy).
rm -f rust-toolchain.toml

echo "Building Wavyzon (jobs: ${CARGO_BUILD_JOBS})..."
cargo build --release --locked -p wavyzon --bin wavyzon

echo "Copying files to /mnt/server..."
cp "${CARGO_TARGET_DIR}/release/wavyzon" /mnt/server/wavyzon
chmod +x /mnt/server/wavyzon
cp LICENSE NOTICE /mnt/server/ 2>/dev/null || true
mkdir -p /mnt/server/data /mnt/server/cache
# Leftovers of the previous egg version, which bundled glibc and started through a script.
rm -rf /mnt/server/lib /mnt/server/start.sh

cd /mnt/server
chmod -R u+w "${BUILD}"
rm -rf "${BUILD}"

if [ ! -x /mnt/server/wavyzon ]; then
    echo "Installation failed"
    exit 1
fi

echo "Installation complete."
exit 0
