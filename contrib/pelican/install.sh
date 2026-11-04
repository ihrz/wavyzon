#!/bin/bash
# Wavyzon - build from source (Rust + CMake for the static libopus), mirrors the repository Dockerfile.
# Built on Debian bookworm; every shared library the binary needs (glibc, libgcc_s...) and the dynamic
# loader are bundled next to it, so the runtime image's glibc does not matter.
# No libdave, no libopus at runtime: DAVE is pure Rust and Opus is linked statically.
set -e
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends cmake clang git ca-certificates

# /tmp is a small tmpfs in the Wings install container: build on the server volume instead.
BUILD=/mnt/server/.build
if [ -d "${BUILD}" ]; then chmod -R u+w "${BUILD}"; rm -rf "${BUILD}"; fi
mkdir -p "${BUILD}/tmp" "${BUILD}/cargo" "${BUILD}/target"
export TMPDIR="${BUILD}/tmp"
export CARGO_HOME="${BUILD}/cargo"
export CARGO_TARGET_DIR="${BUILD}/target"
export CARGO_INCREMENTAL=0
# Fewer parallel jobs = less RAM. A release build needs ~1.5 GB per job.
export CARGO_BUILD_JOBS="${WAVYZON_BUILD_JOBS:-2}"
# The image's rustup/cargo live in /usr/local; keep its toolchain.
export PATH="/usr/local/cargo/bin:${PATH}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
cargo --version

REPO="${WAVYZON_REPO}"
if [ -n "${WAVYZON_GIT_TOKEN}" ]; then
    # Private repository: https://oauth2:<token>@gitlab.com/...
    REPO="$(printf '%s' "${REPO}" | sed -E "s#^https://#https://oauth2:${WAVYZON_GIT_TOKEN}@#")"
fi
echo "Cloning ${WAVYZON_REPO} (${WAVYZON_REF})..."
if ! GIT_TERMINAL_PROMPT=0 git clone --depth 1 --branch "${WAVYZON_REF}" "${REPO}" "${BUILD}/src"; then
    echo "ERROR: could not clone ${WAVYZON_REPO} at ${WAVYZON_REF}."
    echo "If the repository is private, set WAVYZON_GIT_TOKEN (GitLab access token with read_repository)."
    exit 1
fi
cd "${BUILD}/src"
echo "Commit: $(git rev-parse --short HEAD)"

# Use the image's stable toolchain instead of downloading the channel pinned in rust-toolchain.toml.
rm -f rust-toolchain.toml

echo "Building Wavyzon (jobs: ${CARGO_BUILD_JOBS})..."
cargo build --release --locked -p wavyzon --bin wavyzon

echo "Copying files to /mnt/server..."
rm -rf /mnt/server/lib
mkdir -p /mnt/server/lib /mnt/server/data /mnt/server/cache
cp "${CARGO_TARGET_DIR}/release/wavyzon" /mnt/server/wavyzon
chmod +x /mnt/server/wavyzon
cp LICENSE NOTICE /mnt/server/ 2>/dev/null || true
ldd /mnt/server/wavyzon | awk '/=> \//{print $3} $1 ~ /ld-linux/{print $1}' | sort -u | while read -r f; do
    cp -L "$f" /mnt/server/lib/
done
# NSS modules must match the bundled libc (DNS lookups).
MULTIARCH="$(gcc -dumpmachine 2>/dev/null || clang -dumpmachine)"
for n in libnss_files.so.2 libnss_dns.so.2 libresolv.so.2; do
    [ -e "/lib/${MULTIARCH}/${n}" ] && cp -L "/lib/${MULTIARCH}/${n}" /mnt/server/lib/ || true
done
if ! ls /mnt/server/lib/ld-linux-*.so* >/dev/null 2>&1; then
    echo "dynamic loader not bundled"
    exit 1
fi
echo "Bundled libraries:"
ls /mnt/server/lib

cat > /mnt/server/start.sh <<'STARTEOF'
#!/bin/bash
cd /home/container
export WAVYZON_ADDRESS="0.0.0.0:${SERVER_PORT}"
export WAVYZON_DATA_DIR="${WAVYZON_DATA_DIR:-/home/container/data}"
mkdir -p "${WAVYZON_DATA_DIR}"
[ -n "${WAVYZON_AUDIO_CACHE_DIR}" ] && mkdir -p "${WAVYZON_AUDIO_CACHE_DIR}"
echo "[egg] log_level=${WAVYZON_LOG_LEVEL:-<unset>} cache_gb=${WAVYZON_AUDIO_CACHE_GB:-<unset>} playback=${WAVYZON_PLAYBACK_SOURCES:-<unset>} address=${WAVYZON_ADDRESS}"
for v in WAVYZON_PASSWORD SPOTIFY_CLIENT_ID SPOTIFY_CLIENT_SECRET WAVYZON_CACHE_SECRET; do
    val="${!v}"
    if [ -n "${val}" ]; then echo "[egg] ${v}=set (${#val} chars)"; else echo "[egg] ${v}=EMPTY"; fi
done
LOADER="$(ls /home/container/lib/ld-linux-*.so* | head -1)"
exec "${LOADER}" --library-path /home/container/lib ./wavyzon
STARTEOF
chmod +x /mnt/server/start.sh

cd /mnt/server
chmod -R u+w "${BUILD}"
rm -rf "${BUILD}"

if [ ! -x /mnt/server/wavyzon ]; then
    echo "Installation failed"
    exit 1
fi

echo "Installation complete."
exit 0
