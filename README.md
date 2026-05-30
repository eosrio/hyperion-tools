# abi-scanner

[![CI](https://github.com/eosrio/abi-scanner/actions/workflows/ci.yml/badge.svg)](https://github.com/eosrio/abi-scanner/actions/workflows/ci.yml)

A high-performance **Antelope ABI scanner**. It extracts every contract ABI version (`setabi`) across a chain's history into a portable, **Elasticsearch-ingestible** snapshot — either by reading the nodeos **state-history log directly off disk** (fastest, no nodeos load) or by streaming **SHiP** from a node/[fleet-router](https://github.com/eosrio/fleet-router).

Built on **[rs_abieos](https://github.com/eosrio/rs-abieos)** (pure-Rust backend — no C++ toolchain).

Output is one [Hyperion](https://github.com/eosrio/hyperion-history-api) abi-index doc per line, ready to `_bulk` into `<chain>-abi-v1`:

```json
{"account":"eosio.token","block":49,"abi":"{...abi json...}","abi_hex":"0e656f…","actions":["transfer","issue","…"],"tables":["accounts","stat"]}
```

## Two modes

| | how | throughput | nodeos load |
|---|---|---|---|
| **`--from-disk`** (recommended) | reads the append-only `chain_state_history.{log,index}` directly, in parallel | **~168k dense blk/s** (24 cores), scales with cores | **none** — read-only file I/O |
| **`--ship`** | streams SHiP from a node or fleet-router | ~5,900 dense blk/s per node (nodeos's single SHiP thread); fan out across a fleet with fleet-router | one SHiP thread per node |

The disk path bypasses nodeos's single-threaded SHiP serializer entirely, so it scales with CPU cores (the work is zlib inflate) with the disk nowhere near its limit. A **full WAX chain** (~437M blocks) snapshots in **well under an hour** on a many-core node, **without touching nodeos**. Use SHiP mode when you can't co-locate with the node's disk.

## Install

Requires a Rust toolchain (1.74+). No C++/clang needed — the pure-Rust abieos backend is used.

```bash
git clone https://github.com/eosrio/abi-scanner
cd abi-scanner
cargo build --release
# binary at target/release/abi-scanner
```

> The `rs_abieos` dependency is pulled from git until its `rust-backend` feature is published to crates.io.

## Usage

### Direct-from-disk (run on the node, or anywhere the state-history dir is mounted)

```bash
# whole chain (end is clamped to the last committed block) -> portable snapshot
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out wax-abi-snapshot.ndjson
```

- `--threads N` parallel readers (each scans a contiguous block range). Throughput scales ~linearly to the core count; don't exceed physical cores.
- Reads only `chain_state_history.{log,index}` (`trace_history.*` is not needed). **Opens read-only**, and the range is clamped to indexed (committed) blocks, so it never races the entry nodeos is appending — it cannot corrupt anything.
- Resume after an interruption by re-running with a higher `--start`.

#### Snapshot-restored nodes → instant current-ABI snapshot

When a node is started **from a chain snapshot**, the state-history plugin emits the *entire chain state as one delta* on the first block after the snapshot (the `Placing initial state in block N` log line). That single block's `account` table holds **every** contract's current ABI — so scanning just that one block yields a complete current-ABI set in seconds, without walking the chain's history:

```bash
# N = the snapshot's head block (from the nodeos "Placing initial state in block N" log line)
abi-scanner --from-disk /data/nodeos/state-history --start N --end N --out current-abis.ndjson
```

Measured on **Telos (Spring 1.2.2)**: a node restored from a ~1.6 GB snapshot produced a ~1.95 GB init-delta entry; abi-scanner extracted **796 contract ABIs from that one block in ~27 s**. That init-delta entry uses a distinct magic and omits the per-entry position suffix — both handled transparently, so snapshot-restored logs read just like genesis-synced ones.

### SHiP (remote node or fleet-router)

```bash
abi-scanner --ship ws://node:8080 --start 2 --end 999999999 --out wax-abi-snapshot.ndjson

# fan out across a fleet of nodes via fleet-router
abi-scanner --ship ws://fleet-router:18080 --start 2 --end 999999999 \
  --connections 16 --in-flight 200 --out wax-abi-snapshot.ndjson
```

### Ingest into Elasticsearch

Each line is a complete abi-index doc (`_id = block + account`). A minimal bulk ingest:

```bash
awk '{print "{\"index\":{\"_id\":\"" NR "\"}}"; print}' wax-abi-snapshot.ndjson \
  | curl -s -H 'content-type: application/x-ndjson' --data-binary @- \
    "http://localhost:9200/wax-abi-v1/_bulk" > /dev/null
```

(Or derive `_id` as `block + account` to match Hyperion exactly.)

## How it works

A state-history log entry is `[header 48B][u32 size][zlib payload][trailing pos 8B]`; the `.index` is one `u64` file-offset per block (O(1) seek). For each block we `inflate` the payload to `table_delta[]` and walk **only the `account` table**, skipping the dense `contract_row` rows by length — a setabi is an `account` row with a non-empty `abi`. The `account` row is parsed by hand (`[variant][name u64][creation_date u32][abi bytes]`), so **no SHiP ABI is required**; rs_abieos is used only for `name_to_string` and `abi_bin_to_json`. SHiP mode does the same, sourcing the `deltas` bytes from the websocket (`fetch_block=0, fetch_traces=0, fetch_deltas=1`) and zero-copy-parsing the result envelope.

## Benchmark

Dense WAX era (~478 deltas/block), single live node:

| | dense throughput | scales? |
|---|---|---|
| Hyperion `abi_scan_mode` (1 worker) | ~81 blk/s | — |
| SHiP, one node | ~5,900 blk/s | ❌ flat 1→8 connections (single `ship-0` thread) |
| **direct-disk, 1 thread** | 11,964 blk/s | — |
| **direct-disk, 24 threads** | **168,142 blk/s** | ✅ ~linear to cores (CPU-bound on inflate) |

## Limitations / roadmap

- `@timestamp` is omitted; the Hyperion abi lookup keys on `block`. Can be added from the block header if needed.
- A direct `--es` bulk sink (instead of NDJSON) is a planned add-on.
- Checkpoint file for fully-automatic resume on very large chains.

## License

MIT — see [LICENSE](LICENSE).
