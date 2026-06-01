# syntax=docker/dockerfile:1
# Hyperion archive-server — the on-demand tiered-storage archive. It serves action act.data and
# delta values straight from frozen state-history logs over HTTP (GET /action, /block, /health;
# POST /actions, POST /deltas). Built as a static musl binary, shipped on a minimal Alpine.
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# rust:alpine targets x86_64-unknown-linux-musl by default; our deps are pure-Rust, so this is a
# fully static binary. Cache mounts keep release rebuilds fast; copy the binary out of the cache
# mount before the stage ends (cache mounts are not persisted into the image).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin archive-server && \
    cp target/release/archive-server /archive-server

FROM alpine:3 AS runtime
RUN adduser -D -u 10001 archive
COPY --from=builder /archive-server /usr/local/bin/archive-server
USER archive
EXPOSE 8080
# SECURITY: archive-server is read-only but UNAUTHENTICATED. Run it on a trusted/internal network,
# behind the Hyperion API — do NOT expose it directly to the public internet.
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s \
  CMD wget -qO- http://127.0.0.1:8080/health >/dev/null 2>&1 || exit 1
ENTRYPOINT ["archive-server"]
# Pass the log dir + abi-index (and optionally --port/--threads) at runtime, e.g.:
#   docker run -p 8080:8080 -v /data/frozen:/data:ro ghcr.io/eosrio/archive-server \
#     --from-disk /data/state-history --abi-index /data/abi-index.ndjson
