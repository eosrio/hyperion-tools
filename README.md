# Hyperion Tools

[![CI](https://github.com/eosrio/hyperion-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/eosrio/hyperion-tools/actions/workflows/ci.yml)

A Rust workspace of high-performance **Antelope state-history tools** — the engine behind [Hyperion](https://github.com/eosrio/hyperion-history-api)'s direct-from-disk indexing and tiered-storage archive (v4.5).

Most of these tools read the nodeos **state-history log straight off disk** (or stream **SHiP**), which bypasses nodeos's single-threaded `ship` serializer — the historic bottleneck — so throughput scales with CPU cores instead of one node thread. (`es-load` is the exception: it ingests the NDJSON the readers produce.) Decoding uses the pure-Rust **[rs_abieos](https://github.com/eosrio/rs-abieos)** backend, so there is **no C++/clang toolchain** to build.

The zero-copy state-history deserializer at the core originated in EOS Rio's **[fleet-router](https://github.com/eosrio/fleet-router)** and is shared here as the `hyperion-ship` library.

## What's in the box

| crate | kind | what it does |
|---|---|---|
| **`hyperion-ship`** | library | The shared SHiP read + decode core: the parallel direct-from-disk state-history reader, the zero-copy trace/delta hand-walk decoders, the block-log reader, and ABI extraction — on the pure-Rust `rs_abieos` backend. |
| **`abi-scanner`** | binary · stable | Extracts every contract ABI version (`setabi`) across a chain's history into a portable, Elasticsearch-ingestible snapshot — via SHiP or directly from the state-history log. |
| **`archive-server`** | binary · v4.5 | On-demand tiered-storage archive: serves action `act.data` and `contract_row` delta values from frozen state-history logs over HTTP, so cold-tier ES docs can drop the heavy payloads and hydrate on read. |
| **`action-proto`** | binary · experimental | Direct-from-disk action reader: decodes `action_traces` from `trace_history` into Hyperion-shaped action NDJSON (or straight to Elasticsearch). The next-gen indexer read path. |
| **`delta-proto`** | binary · experimental | Direct-from-disk delta reader: decodes `contract_row` table deltas from `chain_state_history` into Hyperion-shaped delta NDJSON. |
| **`es-load`** | binary · tooling | Fast, multi-threaded NDJSON → Elasticsearch `_bulk` loader for measuring the ES write ceiling. Loopback-only by default. |
| **`slice-log`** | binary · tooling | Extracts a rebased block-range slice of a state-history ship log (or the block log), read-only, for local ground-truth testing of the direct-from-disk tools. |

> **Maturity:** `abi-scanner` is production-ready (it builds the published ABI snapshots). `archive-server` powers the v4.5 tiered-storage path. `action-proto`/`delta-proto` are the direct-from-disk indexer **prototypes** — the road to replacing the `ship-0` serializer entirely. `es-load`/`slice-log` are local benchmarking / test-fixture tooling.

## Build

Requires a Rust toolchain (1.74+). No C++/clang needed — the pure-Rust abieos backend is used, and every dependency (including [`rs_abieos`](https://crates.io/crates/rs_abieos)'s `rust-backend`) comes from crates.io. No git dependencies.

```bash
git clone https://github.com/eosrio/hyperion-tools
cd hyperion-tools
cargo build --release
# all binaries land in target/release/: abi-scanner, archive-server, action-proto, delta-proto, es-load, slice-log
```

Build a single tool with `cargo build --release --bin <name>`. The repo is a Cargo workspace; the shared `hyperion-ship` library lives in `crates/core`, and each binary is its own crate under `crates/`.

---

## `abi-scanner` — the ABI index builder

Extracts every contract ABI version (`setabi`) across a chain's history into a portable, **Elasticsearch-ingestible** snapshot — either by reading the nodeos **state-history log directly off disk** (fastest, no nodeos load) or by streaming **SHiP** from a node/[fleet-router](https://github.com/eosrio/fleet-router).

Output is one [Hyperion](https://github.com/eosrio/hyperion-history-api) abi-index doc per line, ready to `_bulk` into `<chain>-abi-v1`:

```json
{"account":"eosio.token","block":49,"abi":"{...abi json...}","abi_hex":"0e656f…","actions":["transfer","issue","…"],"tables":["accounts","stat"]}
```

### Two modes

| | how | throughput | nodeos load |
|---|---|---|---|
| **`--from-disk`** (recommended) | reads the append-only `chain_state_history.{log,index}` directly, in parallel | **~168k dense blk/s** (24 cores), scales with cores | **none** — read-only file I/O |
| **`--ship`** | streams SHiP from a node or fleet-router | ~5,900 dense blk/s per node (nodeos's single SHiP thread); fan out across a fleet with fleet-router | one SHiP thread per node |

The disk path bypasses nodeos's single-threaded SHiP serializer entirely, so it scales with CPU cores (the work is zlib inflate) with the disk nowhere near its limit. A **full WAX chain** (~437M blocks) snapshots in **well under an hour** on a many-core node, **without touching nodeos**. Use SHiP mode when you can't co-locate with the node's disk.

### Direct-from-disk (run on the node, or anywhere the state-history dir is mounted)

```bash
# whole chain (end is clamped to the last committed block) -> portable snapshot
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out wax-abi-snapshot.ndjson
```

- `--threads N` parallel readers pull small chunks from a shared cursor (work-stealing), so every thread stays busy to the end even though recent blocks are far denser than early ones. Throughput scales ~linearly to the core count; don't exceed physical cores.
- Reads only `chain_state_history.{log,index}` (`trace_history.*` is not needed). **Opens read-only**, and the range is clamped to indexed (committed) blocks, so it never races the entry nodeos is appending — it cannot corrupt anything.

#### Resumable scans (`--checkpoint`)

For long full-chain scans, pass `--checkpoint <file>` to make the scan **stop-and-continue from any block**. The scanner records how far it is *contiguously* done; if it's interrupted (Ctrl-C, crash, reboot), **re-run the exact same command** and it picks up where it left off, appending to the same output:

```bash
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out wax-abi.ndjson --checkpoint wax.ckpt
# ...interrupted... just run the same line again — it resumes from the checkpoint:
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out wax-abi.ndjson --checkpoint wax.ckpt
```

Once complete, re-running is a no-op. To **catch up new blocks** later (the chain advanced), re-run the same command: it resumes from the prior end and indexes only the new blocks. Blocks scanned-but-not-yet-checkpointed at the moment of an interruption are re-scanned on resume — harmless, since abi-index docs are keyed by `block + account` (idempotent on `_bulk`).

#### Snapshot-restored nodes → instant current-ABI snapshot

When a node is started **from a chain snapshot**, the state-history plugin emits the *entire chain state as one delta* on the first block after the snapshot (the `Placing initial state in block N` log line). That single block's `account` table holds **every** contract's current ABI — so scanning just that one block yields a complete current-ABI set in seconds, without walking the chain's history:

```bash
# N = the snapshot's head block (from the nodeos "Placing initial state in block N" log line)
abi-scanner --from-disk /data/nodeos/state-history --start N --end N --out current-abis.ndjson
```

Measured on **Telos (Spring 1.2.2)**: a node restored from a ~1.6 GB snapshot produced a ~1.95 GB init-delta entry; abi-scanner extracted **all 796 contract ABIs from that one block**. Entries this large (≥ `--stream-threshold`, default 16 MiB) are **stream-inflated only up to the account table and then skipped**, so the scan uses **bounded memory (~13 MB instead of ~3.7 GB)** and, on a cold read, fetches only the account-table prefix instead of the whole 1.95 GB. The init-delta entry also uses a distinct magic and omits the per-entry position suffix — both handled transparently, so snapshot-restored logs read just like genesis-synced ones.

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

### How it works

A state-history log entry is `[header 48B][u32 size][zlib payload][trailing pos 8B]`; the `.index` is one `u64` file-offset per block (O(1) seek). For each block we `inflate` the payload to `table_delta[]` and walk **only the `account` table**, skipping the dense `contract_row` rows by length — a setabi is an `account` row with a non-empty `abi`. The `account` row is parsed by hand (`[variant][name u64][creation_date u32][abi bytes]`), so **no SHiP ABI is required**; rs_abieos is used only for `name_to_string` and `abi_bin_to_json`. SHiP mode does the same, sourcing the `deltas` bytes from the websocket (`fetch_block=0, fetch_traces=0, fetch_deltas=1`) and zero-copy-parsing the result envelope.

### Benchmark

Dense WAX era (~478 deltas/block), single live node:

| | dense throughput | scales? |
|---|---|---|
| Hyperion `abi_scan_mode` (1 worker) | ~81 blk/s | — |
| SHiP, one node | ~5,900 blk/s | ❌ flat 1→8 connections (single `ship-0` thread) |
| **direct-disk, 1 thread** | 11,964 blk/s | — |
| **direct-disk, 24 threads** | **168,142 blk/s** | ✅ ~linear to cores (CPU-bound on inflate) |

### Limitations

- `@timestamp` is omitted; the Hyperion abi lookup keys on `block`. Can be added from the block header if needed.
- A direct `--es` bulk sink (instead of NDJSON) is a planned add-on.
- `--checkpoint` resume is `--from-disk` only; SHiP-mode resume is not yet wired up.

---

## `archive-server` — the tiered-storage archive

The HTTP server behind Hyperion v4.5 **tiered storage**. Cold-tier ES documents drop the heavy payloads (`act.data`, `contract_row` values); the API hydrates them on read by asking the archive, which decodes them on demand from the frozen state-history logs:

```bash
archive-server --from-disk /data/nodeos/state-history \
  --abi-index wax-abi.ndjson --port 8088
```

| endpoint | purpose |
|---|---|
| `GET /action?block_num=<N>&global_sequence=<G>` | one action's decoded `act.data` |
| `GET /block/<N>` | a block's decoded traces |
| `POST /actions` | batch-hydrate many actions in one round-trip (request order preserved) |
| `POST /deltas` | batch-hydrate many `contract_row` delta values |
| `GET /health` | status + the archived block ranges served (actions, and deltas or `null`) |

`GET /health` reports the coverage this node can serve, so integrators can discover the range instead of probing for it — `deltas` is `null` on a node with no `chain_state_history` log:

```json
{"status":"ok","actions":{"first_block":190373745,"last_block":190374244},"deltas":{"first_block":190373745,"last_block":190374244}}
```

The batch endpoints group requested positions by block so each block is read and decoded exactly once per request (shared per-thread cache), and process blocks in ascending order for sequential disk reads and deterministic results under the per-request work cap. See the Hyperion **tiered-storage** docs for the full wire contract and the API-side hydration flow.

## `action-proto` / `delta-proto` — direct-from-disk readers (experimental)

The prototype read path for the next-gen indexer: decode Hyperion-shaped action/delta documents straight from the on-disk logs, at core-scaling throughput, with no `ship-0` serializer in the loop. Both emit NDJSON (or, for `action-proto`, straight to Elasticsearch) and support a **cold-tier metadata-only** mode that omits the heavy payload the `archive-server` re-serves on demand.

```bash
action-proto --from-disk /data/nodeos/state-history --abi-index wax-abi.ndjson \
  --start 2 --end 999999999 --threads 12 --out wax-actions.ndjson
delta-proto  --from-disk /data/nodeos/state-history --abi-index wax-abi.ndjson \
  --start 2 --end 999999999 --threads 12 --out wax-deltas.ndjson
```

## `es-load` — Elasticsearch write-ceiling benchmark

Fast, multi-threaded `_bulk` loader (real OS-thread posters) that applies Hyperion's `_id`/`_index` rules — fast enough that ES, not the loader, is the bottleneck, so it measures the true write ceiling (typically ~3–4× slower than decode). **Loopback-only by default** — it refuses a non-local ES target unless `BENCH_ALLOW_EXTERNAL_ES=1` is set, so a benchmark can never accidentally hit production. (`bench/` also ships a small Python `bulk-load.py` for quick checks, but it's GIL-bound — use `es-load` for the actual ceiling.)

## `slice-log` — local test fixtures

Extracts a small, self-contained block-range slice of a real ship/block log with a **rebased index**, read-only on the source (only ever reads committed blocks well below the head). Copy a slice off a production node to test the direct-from-disk tools locally, with ground-truth data, free of any node dependency.

```bash
slice-log --dir /data/nodeos/state-history --stem chain_state_history \
  --start 380000000 --count 500 --out ./fixture
```

## License

MIT — see [LICENSE](LICENSE).
