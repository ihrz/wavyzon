# syntax=docker/dockerfile:1.7

# ---- build -----------------------------------------------------------------
FROM rust:1-bookworm AS build

# cmake + a C compiler build libopus from source; it is linked statically
# (see .cargo/config.toml), so the runtime image needs no libopus.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Build with the toolchain the image ships instead of letting rustup download
# the channel pinned in rust-toolchain.toml.
RUN rm -f rust-toolchain.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p wavyzon \
 && install -D -m 0755 target/release/wavyzon /out/wavyzon

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim

ARG VERSION=dev
LABEL org.opencontainers.image.title="Wavyzon" \
      org.opencontainers.image.description="A standalone audio node for Discord bots, written in Rust" \
      org.opencontainers.image.source="https://gitlab.com/ihrz/wavyzon" \
      org.opencontainers.image.url="https://gitlab.com/ihrz/wavyzon" \
      org.opencontainers.image.licenses="CC-BY-NC-SA-4.0" \
      org.opencontainers.image.vendor="iHorizon" \
      org.opencontainers.image.version="${VERSION}"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 wavyzon \
 && useradd --system --uid 10001 --gid wavyzon --home-dir /var/lib/wavyzon --shell /usr/sbin/nologin wavyzon \
 && install -d -o wavyzon -g wavyzon /var/lib/wavyzon

COPY --from=build /out/wavyzon /usr/local/bin/wavyzon
COPY LICENSE NOTICE /usr/share/doc/wavyzon/

ENV WAVYZON_ADDRESS=0.0.0.0:2333 \
    WAVYZON_DATA_DIR=/var/lib/wavyzon \
    HEALTHCHECK_PORT=2333

USER wavyzon
WORKDIR /var/lib/wavyzon
VOLUME ["/var/lib/wavyzon"]
EXPOSE 2333

# Every HTTP endpoint (including /v1/info) needs the Authorization header, so
# the health check only verifies that the port accepts TCP connections, using
# bash's /dev/tcp (no curl in the image). If you change the port in
# WAVYZON_ADDRESS, set HEALTHCHECK_PORT to match.
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 \
  CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${HEALTHCHECK_PORT}"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/wavyzon"]
