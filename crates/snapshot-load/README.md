# snapshot-load

Decode active **contract-table state** directly from an Antelope portable snapshot (`.bin` / `.bin.zst`)
and emit Hyperion-shaped NDJSON — **no nodeos, no SHiP replay**, fully deterministic point-in-time state.

This is the fast, deterministic alternative to `hyp-control sync`, which makes ~one HTTP call per account
against a live node (`get_currency_balance`, `get_account`, paginated `get_table_rows`). Reading the
snapshot off disk is CPU-bound, parallel, and produces a consistent point-in-time view by construction.

The binary format is reverse-engineered and source-cited in [`FORMAT.md`](./FORMAT.md) (verified against
AntelopeIO/spring).

## Supported formats

| `chain_snapshot_header.version` | Era | Contract-table layout | Status |
|---|---|---|---|
| **2–6** | leap 1.x–5.x (pre-Savanna) | single commingled `contract_tables` section | ✅ supported |
| 7 | Spring 1.0.0 (transient) | split per-table sections | ❌ rejected (Spring 1.0.1+ rejects it too) |
| **8** | Spring 1.0.1+ (Savanna) | split per-table sections | ✅ supported |

Input may be a bare `.bin` or a `.bin.zst` (decompressed in-process via pure-Rust `ruzstd`). The reader
branches on the chain version read from the first section; the file-format version (offset-4 `u32`) is a
separate constant (`= 1`) and must not be confused with the chain version.

## Validation matrix

Decode correctness is validated two ways. For **vanilla** chains we diff our output **byte-for-byte**
against `spring-util snapshot to-json` (nodeos's own reference decoder). For **forks** that vanilla
spring-util can't fully re-serialize (their snapshots load, but `to-json` aborts on custom protocol-feature
digests), we rely on the in-tool **structural invariants** + **100 % ABI-decode coverage** +
`spring-util snapshot info` header cross-check.

In-tool invariants (hard errors, never silent garbage):
- **Full-section consumption** — the contract-table walk must land on the section's exact byte boundary (a
  wrong secondary-index skip size desynchronises the walk).
- **Count match (v6)** — every `table_id_object.count` must equal the sum of rows across all 6 index groups.
- **Strictly-increasing `t_id` (v8)** — the `key_value` section's flattened table ids must increase.

| Chain | Net | Ver | Layout | Fork specifics | Validation | Result |
|---|---|---|---|---|---|---|
| **Telos** | mainnet | v6 | commingled | — (vanilla) | spring-util `to-json` byte-diff, **1,389,468 rows** + invariants | ✅ byte-exact |
| **Jungle4** | testnet | v8 | split | — (vanilla) | spring-util `to-json` byte-diff, **75,849 rows** + invariants | ✅ byte-exact |
| **Ultra** | mainnet | v8 | split | custom section `account_free_actions_object`; custom protocol features | invariants + **100 % decode** + spring-util `info` | ✅ |
| **FIO** | mainnet | v2 | commingled | custom section `fioaction_object` + `genesis_state` | invariants (**count==Σ across 2,484,742 tables**) + **100 % decode** + spring-util `info` | ✅ |

Forks customize by **adding** chain objects/sections and protocol features — they do not change the core
snapshot framing or the standard `account_object` / `table_id_object` / `key_value` layouts. Custom sections
are skipped by the section walker, and since they are not contract index types they don't perturb the
table↔`key_value` join. (Vanilla spring-util successfully *loads* Ultra's full state — it only aborts on an
Ultra-custom protocol-feature digest during the write-back, which is unrelated to snapshot reading.)

## Usage

```bash
# decode voters + token balances from a portable snapshot, NDJSON to stdout
snapshot-load --snapshot snapshot-<id>.bin

# a compressed snapshot, written to a file, with 16 decode workers
snapshot-load --snapshot snapshot-<id>.bin.zst --out state.ndjson --threads 16

# diagnose a non-vanilla chain: dump the section list + chain version, then exit
snapshot-load --snapshot snapshot-<id>.bin --inspect

# validate framing + ABI coverage fast (decodes everything, writes nothing)
snapshot-load --snapshot snapshot-<id>.bin --stats-only

# emit raw value hex instead of ABI-decoding (for a byte-level diff vs spring-util to-json)
snapshot-load --snapshot snapshot-<id>.bin --raw --tables voters
```

NDJSON emits the **exact Hyperion doc shapes** for the special targets (`IVoter` for `eosio:voters`,
`IAccount` for `*:accounts`, `IProposal` for `eosio.msig:proposal` with `packed_transaction` fully
unpacked + per-action decoded); every other table emits the dynamic contract-state doc (`@`-prefixed
system fields spread with the decoded row). `--raw` still emits the generic hex line for byte-diffing.

> **Proposals in NDJSON:** the `proposal`↔`approvals2` join (`version`, `requested_approvals`,
> `provided_approvals`) is performed only in the **`--mongo` sink**, which buffers and merges by
> `(proposer, proposal_name)` at end-of-stream. In the default NDJSON sink the `approvals2` carrier
> docs are **dropped** (never emitted), and each `proposal` line is the partial `IProposal` (proposer,
> proposal_name, unpacked `trx`/`expiration`) **without** those three approval fields. For full
> `IProposal` approvals, use the `--mongo` sink.

### MongoDB sink (high-throughput, parallel)

Add `--mongo` to write straight into MongoDB instead of NDJSON (db = `<prefix>_<chain>`, collections
`voters` / `accounts` / `proposals` / `${code}-${table}`). The sink mirrors es-load's "saturate the
sink" model: many concurrent `insert_many(ordered(false))` writers over one pooled async `Client`
(`w:1`, no journal wait), large batches, and **indexes built after the bulk load**. Requires `mongo:8`
(or newer) and the `mongodb` 3.7 Rust driver.

```bash
# decode + index all Telos token balances straight into a local mongo:8
snapshot-load --snapshot snapshot-<id>.bin --chain telos --tables accounts \
  --mongo "mongodb://localhost:27017" --mongo-writers 16 --mongo-batch 4000 --mongo-drop

# voters + balances + msig proposals (proposals are joined to approvals2 for version/approvals)
snapshot-load --snapshot snapshot-<id>.bin --chain telos \
  --tables "voters,accounts,eosio.msig:proposal,eosio.msig:approvals2" \
  --mongo "mongodb://user:pass@host:27017" --mongo-auth-source admin
```

| flag | default | meaning |
|---|---|---|
| `--mongo <uri>` | — | `mongodb://[user:pass@]host:port`; presence switches the sink from NDJSON to Mongo |
| `--chain <name>` | — | **required** with `--mongo`; db name = `<mongo-prefix>_<chain>` |
| `--mongo-prefix` | `hyperion` | database_prefix |
| `--mongo-auth-source` | — | auth database, applied via the typed credential source (not appended to the URI) |
| `--mongo-writers` | `8` | concurrent `insert_many` futures in flight |
| `--mongo-batch` | `4000` | docs per `insert_many` |
| `--mongo-pool` | writers+2 | max connection pool size |
| `--mongo-drop` | off | drop target collections before load (idempotent re-runs) |
| `--mongo-no-index` | off | skip post-load index build (pure write-ceiling benchmarking) |

The `serde_json::Value → BSON` encode happens in the parallel decode workers (not the single batch
accumulator), so the concurrent writers actually saturate the sink. The run reports decode+write
`docs/s`, per-collection counts, the index-build time, and grand-total wall-clock — the end-to-end
"index a full table set" number. Measured locally against a throwaway
`docker run --rm -p 27017:27017 mongo:8`: **Telos `accounts` — 1,145,318 validated token-balance docs,
write 1.8 s → ~623 K docs/s, indexes 3.5 s, grand total 5.4 s, 0 errors** (writers=24, batch=6000;
only validated token contracts are written, mirroring `sync-accounts` `scanABIs`).

`block_num` is derived from the snapshot filename (EOSUSA `snapshot-<64-hex block_id>.bin` → first 4 bytes
of the block_id; EOS Nation `snapshot-...-<decimal>.bin[.zst]` → trailing digits). When streaming a
`.tar.gz` it is derived instead from the **tar's inner `snapshot-<block_id>.bin` entry name** — so a
"latest" pointer like `latest.tar.gz` (whose URL basename carries no block id) still resolves. Pass
`--block-num` to override.

## Streaming directly from a download (`--snapshot-url`)

Instead of `--snapshot <file>`, point at a URL with `--snapshot-url` to **decode + index straight off the
HTTP download** — no separate download/extract step. One forward pass overlaps download, decompression
(gunzip for `.tar.gz`/`.tgz`, streaming zstd for `.bin.zst`/`.zst`), decode and the Mongo writes, so a
live Hyperion can be spun up from current state in roughly the time it takes to pull the snapshot once.
The streamed pipeline reuses the exact same workers + sink as the seek path and is validated
byte-identical to it.

```bash
# stream a "latest" snapshot straight into a local mongo:8 — block_num comes from the tar entry name
snapshot-load --snapshot-url https://example.org/snaps/telos/latest.tar.gz \
  --chain telos --tables accounts --mongo "mongodb://localhost:27017" --mongo-drop

# stream + decode to NDJSON while also saving the raw .bin for nodeos (--tee mirrors every byte)
snapshot-load --snapshot-url https://example.org/snaps/telos/latest.tar.gz \
  --tee ./snapshot.bin --out state.ndjson
```

`--snapshot-url` accepts `.tar.gz` | `.tgz` | `.bin.zst` | `.zst` | `.bin`. The stream is forward-only, so
`--inspect` (which needs random access) is unavailable in this mode. `--tee <path>` writes the raw
decompressed `.bin` alongside indexing (it forces a full read to EOF so the saved file is complete).

## Architecture

A single **producer** thread scans the file sequentially (the framing is length-prefixed, so the scan
can't be parallelised) and pushes owned rows onto a bounded channel; **N decode workers** each own an
`rs_abieos` `AbiHandle` registry (`Send`, not `Sync`) and decode in parallel; one **writer** drains NDJSON.
Bounded channels provide backpressure → bounded memory for EOS-scale (18 GB+) snapshots. ABIs are read once
from the `account_object` section into a shared read-only map; each worker lazily parses the handles it needs.

When `--mongo` is set the single NDJSON writer is replaced by a **bridge thread** that runs a tokio
runtime and drives `--mongo-writers` concurrent `insert_many(ordered(false))` futures over one pooled
`Client` (`buffer_unordered`), accumulating per-collection batches off the writers' hot path. Decode
workers build the exact-shaped `serde_json::Value` docs in parallel and send `(collection, doc)` on the
typed channel. Proposals are buffered + merged with `approvals2` at end-of-stream; indexes build last.

```
src/
├── main.rs     # CLI, decompress, enumerate, version dispatch, pipeline wiring (NDJSON or Mongo sink)
├── reader.rs   # Snap (header + section framing, seek/skip), enumerate_sections
├── model.rs    # RawRow, Targets, AbiRegistry (table + action decode, token-contract check), load_abis
├── tables.rs   # v6 (commingled) + v8 (split) contract-table walkers (producers)
├── map.rs      # decoded row -> exact IVoter/IAccount/IProposal + dynamic doc; packed_transaction decode
├── mongo.rs    # high-throughput parallel MongoDB sink (pooled client, batched insert_many, indexes)
└── perms.rs    # permissions decode (native sections; exact IPermission)
abis/
└── transaction.abi.json  # embedded ABI used to unpack eosio.msig proposal.packed_transaction
```

## `hyp-control sync` parity

All five sync targets are covered. Contract-table targets decode through one generic walker
(`--tables` = `voters` | `accounts` | `eosio.msig:proposal` | `code:scope:table` | `*` …); `permissions`
is a dedicated path over the native sections.

| `hyp-control sync` target | snapshot-load | output shape |
|---|---|---|
| `voters` | `--tables voters` | **exact `IVoter`** (`is_proxy` bool, weights as strings, `staked` number, `primary_key`) |
| `accounts` (balances) | `--tables accounts` | **exact `IAccount`** (`symbol`+`amount` from the asset; only validated token contracts, mirroring `scanABIs`) |
| `proposals` (eosio.msig) | `--tables "eosio.msig:proposal,eosio.msig:approvals2"` | **exact `IProposal`** — `packed_transaction` unpacked + each action's `data` decoded against its contract ABI; `expiration` a BSON `Date`; `version`/approvals joined from `approvals2` |
| dynamic `contract-state` | `--tables code:scope:table` / `*` | dynamic contract-state doc (`@scope/@pk/@payer/@block_num/@block_id/@block_time` + spread `data`) |
| `permissions` | `--tables permissions` | **exact `IPermission`** (authority + `linked_actions`); validated on Telos — 2,264,385 perms, 100% authority decode, full-section consumption |

`permissions` renders the `authority` (keys as `PUB_K1_…`) and `last_updated` via the eosio ABI's
`authority` / built-in `time_point` types, and joins `permission_link_object` for `linked_actions`.

All five sync targets now emit exact Hyperion doc shapes, to NDJSON or straight into MongoDB
(`--mongo`). For proposals, **select `approvals2` alongside `proposal`** so `version`,
`requested_approvals` and `provided_approvals` are joined in by `(proposer, proposal_name)`; a proposal
with no matching `approvals2` row simply omits those three fields (matching `sync-proposals.ts`).

## Reproducing the validation

Helper scripts live under `snapshots/` (gitignored): `tojson.sh` (run spring-util to-json), `diff.py`
(byte-diff our `--raw` output vs the oracle JSON), `inspect.sh` (section/row-format dump). spring-util is
obtained without sudo by extracting the release `.deb` (`dpkg-deb -x antelope-spring_*.deb ~/spring`).
