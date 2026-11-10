# abi-scanner — the ABI index builder

Extracts every contract ABI version (`setabi`) across a chain's history into a portable,
**Elasticsearch-ingestible** snapshot — either by reading the nodeos **state-history log directly off
disk** (fastest, no nodeos load) or by streaming **SHiP** from a node /
[fleet-router](https://github.com/eosrio/fleet-router).

The default Antelope output is one [Hyperion](https://github.com/eosrio/hyperion-history-api) abi-index doc per line, ready to
`_bulk` into `<chain>-abi-v1`:

```json
{"account":"eosio.token","block":49,"abi":"{...abi json...}","abi_hex":"0e656f…","actions":["transfer","issue","…"],"tables":["accounts","stat"]}
```

## Antelope modes

| | how | throughput | nodeos load |
|---|---|---|---|
| `--from-disk` (recommended) | reads the append-only `chain_state_history.{log,index}` directly, in parallel | ~168k dense blk/s (24 cores), scales with cores | none — read-only file I/O |
| `--ship` | streams SHiP from a node or fleet-router | ~5,900 dense blk/s per node (nodeos's single SHiP thread); fan out across a fleet with fleet-router | one SHiP thread per node |

The disk path bypasses nodeos's single-threaded SHiP serializer entirely, so it scales with CPU cores (the
work is zlib inflate) with the disk nowhere near its limit. A full WAX chain (~437M blocks) snapshots in
well under an hour on a many-core node, without touching nodeos. Use SHiP mode when you can't co-locate
with the node's disk.

## Direct-from-disk (run on the node, or anywhere the state-history dir is mounted)

```bash
# whole chain (end is clamped to the last committed block) -> portable snapshot
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out abi-snapshot.ndjson
```

- `--threads N` parallel readers pull small chunks from a shared cursor (work-stealing), so every thread
  stays busy to the end even though recent blocks are far denser than early ones. Throughput scales
  ~linearly to the core count; don't exceed physical cores.
- Reads only `chain_state_history.{log,index}` (`trace_history.*` is not needed). Opens read-only, and the
  range is clamped to indexed (committed) blocks, so it never races the entry nodeos is appending — it
  cannot corrupt anything.

### Resumable scans (`--checkpoint`)

For long full-chain scans, pass `--checkpoint <file>` to make the scan stop-and-continue from any block.
The scanner records how far it is *contiguously* done; if it's interrupted (Ctrl-C, crash, reboot),
re-run the exact same command and it picks up where it left off, appending to the same output:

```bash
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out abi.ndjson --checkpoint chain.ckpt
# ...interrupted... just run the same line again — it resumes from the checkpoint:
abi-scanner --from-disk /data/nodeos/state-history --start 2 --end 999999999 \
  --threads 12 --out abi.ndjson --checkpoint chain.ckpt
```

Once complete, re-running is a no-op. To catch up new blocks later (the chain advanced), re-run the same
command: it resumes from the prior end and indexes only the new blocks. Blocks scanned-but-not-yet-
checkpointed at the moment of an interruption are re-scanned on resume — harmless, since abi-index docs
are keyed by `block + account` (idempotent on `_bulk`).

### Snapshot-restored nodes → instant current-ABI snapshot

When a node is started from a chain snapshot, the state-history plugin emits the *entire chain state as one
delta* on the first block after the snapshot (the `Placing initial state in block N` log line). That single
block's `account` table holds every contract's current ABI — so scanning just that one block yields a
complete current-ABI set in seconds, without walking the chain's history:

```bash
# N = the snapshot's head block (from the nodeos "Placing initial state in block N" log line)
abi-scanner --from-disk /data/nodeos/state-history --start N --end N --out current-abis.ndjson
```

Measured on Telos (Spring 1.2.2): a node restored from a ~1.6 GB snapshot produced a ~1.95 GB init-delta
entry; abi-scanner extracted all 796 contract ABIs from that one block. Entries this large
(≥ `--stream-threshold`, default 16 MiB) are stream-inflated only up to the account table and then skipped,
so the scan uses bounded memory (~13 MB instead of ~3.7 GB) and, on a cold read, fetches only the
account-table prefix instead of the whole 1.95 GB. The init-delta entry also uses a distinct magic and
omits the per-entry position suffix — both handled transparently, so snapshot-restored logs read just like
genesis-synced ones.

## SHiP (remote node or fleet-router)

```bash
abi-scanner --ship ws://node:8080 --start 2 --end 999999999 --out abi-snapshot.ndjson

# fan out across a fleet of nodes via fleet-router
abi-scanner --ship ws://fleet-router:18080 --start 2 --end 999999999 \
  --connections 16 --in-flight 200 --out abi-snapshot.ndjson
```

## Wire: complete deployment history from frozen disk logs

Wire disk mode uses an explicitly selected, source-pinned protocol provider and both
`chain_state_history.{log,index}` and `trace_history.{log,index}`. It reads each canonical
index slot, rather than abandoned physical fork entries. It does not use Antelope's
account-first delta shortcut. Payloads are read under the pinned Wire codec's rules, including
its correction of the evidenced resource-index table-count defect; unrelated malformed transport
data still fails the scan.

Freeze a consistent, read-only view of both logs and indices first. Obtain the chain ID,
an irreversible block number/ID, and the exact SHiP handshake from that node. A scan from
genesis requires full state coverage from block 1 and trace coverage from block 1 or 2 through
the specified boundary. A missing genesis trace is recorded explicitly with the selected native
profile as the sequence-zero baseline. Other partial ranges and snapshot-only initial state
cannot establish complete action ABI epochs and are refused; a sealed catalog is extended
instead (below).

```bash
abi-scanner --protocol wire-sysio-6508c9e8-engineering-v1 \
  --from-disk /data/frozen-wire-state-history --start 1 --end "$LIB" \
  --ship-abi wire-ship-schema.json --chain-id "$CHAIN_ID" \
  --irreversible-block-id "$LIB_ID" --out wire-abis.ndjson
```

`--threads` reader threads (default: every available core) each take a run of 2,048 blocks,
inflate its chain-state and trace payloads and walk their bytes in place, keeping only what can
change the history. One fold applies those facts in block order, so the output does not depend
on the thread count. A walk accepts and refuses the same bytes as the pinned codec, including its
16 MiB input and per-array limits, but renders no JSON. It therefore does not apply the codec's
JSON traversal bounds: a valid block with tens of thousands of actions is read rather than
refused. The SHiP handshake must define every walked type exactly as the walk reads it.
`--checkpoint` is refused, because a catalog is sealed atomically: extend it instead of resuming.
This mode does not decode ordinary contract action values or write per-block Elasticsearch
records. On a frozen copy of the Wire testnet logs (3.6 million blocks, 44 GB) a scan from
genesis takes about 21 seconds on 32 threads. The previous scanner rendered every payload as JSON
and took about 51 minutes for byte-identical output.

Successful primary `sysio::setabi` traces produce deployment records with transaction ID,
action ordinal, global sequence and the target account's ABI revision. Revisions count from
genesis, including same-value deployments and clears, and are checked against ordinary primary
action receipts. Notifications do not add deployments. The current native observer's qualified
scope is executed, unscheduled transactions without failed-deferred traces; other
forms fail explicitly. In particular, soft-fail/onerror can commit effects and must not be
silently skipped. Schema-decoded `partial_transaction_v0` header/signature metadata is
accepted, as evidenced in the real captured Wire trace; it does not replace `action_traces`.
Block-final
account deltas independently validate the final value and produce sparse baseline/change/clear
barriers. Malformed contract ABI bytes are retained with `abiDecodeError`; a contract decoding
failure does not invalidate otherwise complete source coverage.

`--out wire-abis.ndjson` creates these fresh files:

- `wire-abis.ndjson`: sparse deployment and block-final records. This is a different format
  from the default Antelope abi-index docs; use the Hyperion Wire catalog consumer.
- `wire-abis.ndjson.blocks.bin`: canonical 32-byte block IDs in ascending order from block 1.
- `wire-abis.ndjson.manifest.json`: `hyperion-wire-abi-history-v1`, chain/node/provider/schema
  identity, source files and ranges, inclusive coverage, irreversible boundary, exact final
  global sequence, record counts, lengths and SHA-256 checksums of both data files.

All sequences are exact decimal strings. Only the manifest published after successful
validation authorizes consumption. Read/checksum failures, source gaps, changed source files,
receipt disagreements and mismatched source identities fail without a completed manifest.
Interrupted `.partial` files remain diagnostic artifacts; rerun with a new output name.
Existing completed files are never overwritten. The chain ID and irreversible witness must
come from the independently verified source; the scanner checks the supplied end ID against
the indexed logs rather than inferring a chain ID from state-history files.

### Extending a sealed catalog

Extend a sealed catalog to a later irreversible block instead of scanning again from genesis:

```bash
abi-scanner --protocol wire-sysio-6508c9e8-engineering-v1   --from-disk /data/frozen-wire-state-history --start "$((BASE_END + 1))" --end "$LIB"   --ship-abi wire-ship-schema.json --chain-id "$CHAIN_ID"   --irreversible-block-id "$LIB_ID" --out "wire-abis-$LIB.ndjson"   --base-manifest wire-abis.ndjson.manifest.json --base-manifest-sha256 "$BASE_SHA256"
```

The base manifest is pinned by its SHA-256, as its readers pin it, and its chain, node
revision, provider profile and SHiP schema must match the run. Each base member is read once:
the bytes checked against the manifest are the bytes copied. Its records rebuild every account's
ABI state at the base's end. The logs must still hold the base's last block ID; if they do not,
the chain forked past it and the extension is refused. The logs need state coverage from the
base's last block and trace coverage from the next one.

Only the blocks after the base are read. The output starts with the base's records and block
IDs, byte for byte, followed by the new ones: the same files a scan from genesis writes. The
manifest matches too, apart from an added `base` entry with the base manifest's SHA-256, end
block and last block ID. Its `sources` list only the logs this run read. On the Wire testnet,
extending a catalog by 111,537 blocks takes under a second.

An account that has only ever held an empty ABI has no records, so a base cannot prove its
state. If such an account is removed after the base, the extension is refused and a scan from
genesis is needed.

### Sealing incremental segments

A complete extension copies every block ID of the chain again (32 bytes per block), so sealing
often that way would rewrite the whole sidecar each time. A **segment** holds only what it adds:

```bash
abi-scanner --protocol wire-sysio-6508c9e8-engineering-v1 --segment   --catalog-dir catalogs --genesis-catalog-sha256 "$GENESIS_SHA256"   --from-disk /srv/wire/state-history --start "$((CHAIN_END + 1))" --end "$LIB"   --ship-abi wire-ship-schema.json --chain-id "$CHAIN_ID" --irreversible-block-id "$LIB_ID"   --out catalogs/segments
```

- The chain is found under `--catalog-dir` from its pinned genesis and verified link by link.
  `--start` must be the chain's end + 1.
- The output is `abi-history.<start>-<end>.ndjson` with its `.blocks.bin` and `.manifest.json`.
  The manifest format is `hyperion-abi-history-segment-v1`. It names its base like an
  extension, and adds `startBlock`, `startBlockId` and `totalRecords` (the chain's records
  through it). Its members hold only the new records and the block IDs of `startBlock..=endBlock`.
- Genesis records followed by every segment's records, and the same for block IDs, are
  byte for byte what a complete scan to the same end writes.
- The logs may be a running node's. Only entries at or below the irreversible end block are
  read, and they are checked as strictly as frozen logs (magic, position trailers, payload
  framing and sizes).

`range-server --seal` runs the same sealer continuously.

## Ingest into Elasticsearch

Each line is a complete abi-index doc (`_id = block + account`). A minimal bulk ingest:

```bash
awk '{print "{\"index\":{\"_id\":\"" NR "\"}}"; print}' abi-snapshot.ndjson \
  | curl -s -H 'content-type: application/x-ndjson' --data-binary @- \
    "http://localhost:9200/<chain>-abi-v1/_bulk" > /dev/null
```

(Or derive `_id` as `block + account` to match Hyperion exactly.)

## How it works

A state-history log entry is `[header 48B][u32 size][zlib payload][trailing pos 8B]`; the `.index` is one
`u64` file-offset per block (O(1) seek). For each block we `inflate` the payload to `table_delta[]` and
walk only the `account` table, skipping the dense `contract_row` rows by length — a setabi is an `account`
row with a non-empty `abi`. The `account` row is parsed by hand
(`[variant][name u64][creation_date u32][abi bytes]`), so no SHiP ABI is required; rs_abieos is used only
for `name_to_string` and `abi_bin_to_json`. SHiP mode does the same, sourcing the `deltas` bytes from the
websocket (`fetch_block=0, fetch_traces=0, fetch_deltas=1`) and zero-copy-parsing the result envelope.

## Benchmark

Dense WAX era (~478 deltas/block), single live node:

| | dense throughput | scales? |
|---|---|---|
| Hyperion `abi_scan_mode` (1 worker) | ~81 blk/s | — |
| SHiP, one node | ~5,900 blk/s | ❌ flat 1→8 connections (single `ship-0` thread) |
| direct-disk, 1 thread | 11,964 blk/s | — |
| direct-disk, 24 threads | 168,142 blk/s | ✅ ~linear to cores (CPU-bound on inflate) |

## Limitations

- `@timestamp` is omitted; the Hyperion abi lookup keys on `block`. Can be added from the block header if
  needed.
- A direct `--es` bulk sink (instead of NDJSON) is a planned add-on.
- `--checkpoint` resume is `--from-disk` only; SHiP-mode resume is not yet wired up.
