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

## Crates

Every crate has its own README with full usage, benchmarks, and internals — follow the links.

### State-history tools

| crate | status | what it does |
|---|---|---|
| [**abi-scanner**](crates/abi-scanner) | stable | Extracts every contract ABI version (`setabi`) across a chain's history into a portable, Elasticsearch-ingestible snapshot — via SHiP or directly off disk. (~168k blk/s on 24 cores.) |
| [**archive-server**](crates/archive-server) | v4.5 | On-demand tiered-storage archive: serves action `act.data` and `contract_row` delta values from frozen logs over HTTP, so cold-tier ES docs can drop the heavy payloads and hydrate on read. |
| [**action-proto**](crates/action-proto) | experimental | Direct-from-disk action reader: decodes `action_traces` into Hyperion-shaped action NDJSON (or straight to Elasticsearch). Next-gen indexer read path. |
| [**delta-proto**](crates/delta-proto) | experimental | Direct-from-disk delta reader: decodes `contract_row` table deltas into Hyperion-shaped delta NDJSON. |

### Light API (snapshot → Mongo → serve)

| crate | status | what it does |
|---|---|---|
| [**snapshot-load**](crates/snapshot-load) | prototype | Decodes active contract-table state straight from an Antelope portable snapshot into Hyperion-shaped NDJSON or MongoDB — no nodeos, no SHiP replay. The deterministic alternative to `hyp-control sync`. |
| [**light-api**](crates/light-api) | — | tokio + axum server reproducing the cc32d9 `eosio_light_api` HTTP API over the per-chain MongoDB `snapshot-load` writes / Hyperion maintains live. |
| [**wseg-build**](crates/wseg-build) | — | Builds a frozen, memory-mappable columnar segment (`.wseg`) of the Light-API tables from the per-chain Mongo, for WormDB to serve at tens-of-MiB resident. |

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

Requires a Rust toolchain (**1.88+**). No C++/clang needed — the pure-Rust abieos backend is used, and
every dependency (including [`rs_abieos`](https://crates.io/crates/rs_abieos)'s `rust-backend`) comes from
crates.io. No git dependencies.

```bash
git clone https://github.com/eosrio/hyperion-tools
cd hyperion-tools
cargo build --release
# binaries land in target/release/
```

Build a single tool with `cargo build --release --bin <name>`. The shared `hyperion-ship` library lives in
`crates/core`; each binary is its own crate under `crates/`.

## License

MIT — see [LICENSE](LICENSE).
