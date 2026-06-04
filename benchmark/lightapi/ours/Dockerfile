# Build context = repo root (see docker-compose.yml: context: ../..). Builds the two binaries our stack
# needs (snapshot-load = loader, light-api = server) and ships them on a slim runtime.
FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Pure-Rust deps (rs_abieos rust-backend, rustls, ruzstd, bs58, ripemd) → no C toolchain needed.
RUN cargo build --release -p snapshot-load -p light-api

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates bash coreutils findutils && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/snapshot-load /usr/local/bin/snapshot-load
COPY --from=builder /build/target/release/light-api /usr/local/bin/light-api
COPY benchmark/ours/light-api.toml /etc/light-api.toml
COPY benchmark/ours/entrypoint-loader.sh /entrypoint-loader.sh
COPY benchmark/ours/entrypoint-server.sh /entrypoint-server.sh
RUN chmod +x /entrypoint-loader.sh /entrypoint-server.sh
