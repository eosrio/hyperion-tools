# syntax=docker/dockerfile:1
# antelope-ship-archive: serves frozen Antelope state-history and block logs over the SHiP
# WebSocket protocol, exactly as a Spring node does, for a SHiP-aware router or proxy. Built as a
# static musl binary, shipped on a minimal Alpine. Build it from the repository root:
#   docker build -f crates/ship-archive/Dockerfile -t antelope-ship-archive .
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# rust:alpine targets x86_64-unknown-linux-musl by default; the server's dependencies are pure
# Rust, so this is a fully static binary. Cache mounts keep release rebuilds fast; copy the
# binary out of the cache mount before the stage ends (cache mounts are not kept in the image).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --package antelope-ship-archive && \
    cp target/release/antelope-ship-archive /antelope-ship-archive

FROM alpine:3 AS runtime
RUN adduser -D -u 10001 ship
COPY --from=builder /antelope-ship-archive /usr/local/bin/antelope-ship-archive
USER ship
# 8080: SHiP (WebSocket). 9180: /health, /ready and /metrics (--http).
EXPOSE 8080 9180
# SECURITY: the server opens every file read-only but has NO authentication. Run it on a private
# network, behind a SHiP-aware router or proxy; do NOT publish it to the internet.
# /ready answers 200 once the logs are validated and the listener is bound, and 503 during
# shutdown. Startup validates every file before it binds, hence the long start period. The
# check needs `--http` on port 9180: keep it when you replace the command below.
HEALTHCHECK --interval=30s --timeout=3s --start-period=60s \
  CMD wget -qO- http://127.0.0.1:9180/ready >/dev/null 2>&1 || exit 1
ENTRYPOINT ["antelope-ship-archive"]
# Mount the frozen logs (state-history and block logs, any layout below /data) read-only, as a
# directory uid 10001 can read, e.g.:
#   docker run -p 8080:8080 -v /srv/archive/seg-0001:/data:ro ghcr.io/eosrio/antelope-ship-archive
# To serve part of the logs, or with other options, give the whole command after the image:
#   ... ghcr.io/eosrio/antelope-ship-archive serve --dir /data --recursive \
#         --range 2-50000001 --listen 0.0.0.0:8080 --http 0.0.0.0:9180
# The offline commands run the same way, e.g. `... check --dir /data --recursive --deep`.
CMD ["serve", "--dir", "/data", "--recursive", "--listen", "0.0.0.0:8080", "--http", "0.0.0.0:9180"]
