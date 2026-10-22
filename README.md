# Hyperion Tools

[![CI](https://github.com/eosrio/hyperion-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/eosrio/hyperion-tools/actions/workflows/ci.yml)

A Rust workspace of high-performance **Antelope state-history tools** — the engine behind
[Hyperion](https://github.com/eosrio/hyperion-history-api)'s direct-from-disk indexing and tiered-storage
archive (v4.5).

Most tools read the nodeos **state-history log straight off disk** (or stream **SHiP**), bypassing
nodeos's single-threaded `ship` serializer — the historic bottleneck — so throughput scales with CPU
cores instead of one node thread. Decoding uses the pure-Rust
[`rs_abieos`](https://github.com/eosrio/rs-abieos) backend, so there is **no C++/clang toolchain** to
build. The zero-copy deserializer at the core originated in EOS Rio's
[fleet-router](https://github.com/eosrio/fleet-router) and ships here as the `hyperion-ship` library.

## Hyperion 4.5 development

The [architecture definition](docs/architecture/hyperion-4.5.md) records the accepted release contracts.
Use the [roadmap](docs/roadmap/hyperion-4.5.md) for repository ownership, milestones and the linked
issue frontier. Track cross-repository delivery in [release issue #24](https://github.com/eosrio/hyperion-tools/issues/24).
These documents distinguish accepted scope from implementation and qualification.
The [TypeScript toolchain policy](docs/architecture/hyperion-4.5-typescript-toolchain.md) defines the
TypeScript 7 and Bun checker baseline for the History API, SDK, and TypeScript extensions.

## Crates

Every crate has its own README with full usage, benchmarks, and internals — follow the links.

### State-history tools

| crate | status | what it does |
|---|---|---|
| [**abi-scanner**](crates/abi-scanner) | stable | Extracts every contract ABI version (`setabi`) across a chain's history into a portable, Elasticsearch-ingestible snapshot — via SHiP or directly off disk. (~168k blk/s on 24 cores.) |
| [**archive-server**](crates/archive-server) | v4.5 | On-demand tiered-storage archive: serves action `act.data` and `contract_row` delta values from frozen logs over HTTP, so cold-tier ES docs can drop the heavy payloads and hydrate on read. |
| [**antelope-ship-archive**](crates/ship-archive) | preview | Serves frozen state-history and block logs over the SHiP WebSocket protocol exactly as a Spring node does, without nodeos, so a SHiP-aware router or proxy can stitch archived ranges and live nodes into one history. Strict, read-only, fail-closed readers; bounded memory; `check` and `index` for preparing archives. |
| [**action-proto**](crates/action-proto) | experimental | Direct-from-disk action reader: decodes `action_traces` into Hyperion-shaped action NDJSON (or straight to Elasticsearch). Next-gen indexer read path. |
| [**delta-proto**](crates/delta-proto) | experimental | Direct-from-disk delta reader: decodes `contract_row` table deltas into Hyperion-shaped delta NDJSON. |
| [**protocol-replay**](crates/protocol-replay) | engineering | Replay complete source-pinned Wire SHiP frames and ABI/value/KV records through the coordinated package's Rust provider, retaining raw bytes and deployment context. |

Wire engineering profiles are explicit `--protocol` selections. The
[Rust provider support matrix](docs/qualification/wire-rust-provider.md) records the working
live ABI scan/replay paths and the disk/bulk/archive paths that currently refuse Wire before
entering incompatible readers. This integration is not selected-testnet qualification.

Deploy frozen SHiP archives beside live nodes with the
[fleet-router v0.7+ topology guide](https://eosrio.github.io/fleet-router/frozen-ship.html) and
[archive preparation instructions](crates/ship-archive/FLEET_ROUTER.md).

### Log recovery

| crate | status | what it does |
|---|---|---|
| [**antelope-log-repair**](crates/log-repair) | experimental | Finds and repairs unreadable records in block and SHIP logs: scans for I/O errors, rebuilds the affected entries from independent SHIP endpoints, another node's blocklog, or an isolated snapshot replay, and writes them back offline after a reviewed plan is approved. |

### Light API (snapshot → Mongo → serve)

| crate | status | what it does |
|---|---|---|
| [**snapshot-load**](crates/snapshot-load) | prototype | Decodes active contract-table state straight from an Antelope portable snapshot into Hyperion-shaped NDJSON or MongoDB — no nodeos, no SHiP replay. The deterministic alternative to `hyp-control sync`. A `--tables lightapi` preset + `--wseg` sink build the whole Light-API dataset (incl. a memory-mapped segment) in one pass. |
| [**light-api**](crates/light-api) | preview | tokio + axum server reproducing the cc32d9 `eosio_light_api` HTTP API over the per-chain MongoDB `snapshot-load` writes / Hyperion maintains live. |
| [**wseg-build**](crates/wseg-build) | preview | Builds a frozen, memory-mappable columnar segment (`.wseg`) of the Light-API tables — from the per-chain Mongo, or directly from a snapshot via `snapshot-load --wseg` — for WormDB to serve the full HTTP + WebSocket Light API at tens-of-MiB resident. |

### Benchmarking & test fixtures

| crate | status | what it does |
|---|---|---|
| [**es-load**](crates/es-load) | tooling | Fast, multi-threaded NDJSON → Elasticsearch `_bulk` loader for measuring the ES write ceiling. Loopback-only by default. |
| [**slice-log**](crates/slice-log) | tooling | Extracts a rebased block-range slice of a state-history ship log (or block log), read-only, for local ground-truth testing of the direct-from-disk tools. |

### Library

| crate | what it does |
|---|---|
| [**hyperion-ship**](crates/core) | The shared SHiP read + decode core: the parallel direct-from-disk reader, the zero-copy trace/delta decoders, the block-log reader, and ABI extraction — on the pure-Rust `rs_abieos` backend. |

## Build

Rust **1.95 or newer** is required. CI checks the minimum compiler and current stable with the
locked `rs_abieos` **0.7.0** pure-Rust backend; no C++/clang is needed. The
[Rust baseline record](docs/qualification/rs-abieos-070.md) identifies the published artifact,
compatibility checks and the historical 1.88 mismatch resolved by this baseline.
The internal Wire codec remains a separate, exact source-pinned Cargo git dependency within
the coordinated Wire package; the other external dependencies come from crates.io.

```bash
git clone https://github.com/eosrio/hyperion-tools
cd hyperion-tools
cargo build --release --locked
# binaries land in target/release/
```

`rust-toolchain.toml` selects current stable for development. To exercise the supported minimum,
install it with `rustup toolchain install 1.95.0 --profile minimal` and use
`cargo +1.95.0 build --release --locked`. Keep `Cargo.lock` when building release artifacts.

Build a single tool with `cargo build --release --bin <name>`. The shared `hyperion-ship` library lives in
`crates/core`; each binary is its own crate under `crates/`.

## License

MIT — see [LICENSE](LICENSE).
