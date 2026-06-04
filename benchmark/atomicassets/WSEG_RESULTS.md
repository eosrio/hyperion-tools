# AtomicAssets state tier — measured comparison (Postgres vs Mongo vs WormDB)

All numbers are **measured on real WAX data** (snapshot block 438147349 for mainnet; 409250749 for
testnet), 2026-06-04, on one box (61 GB RAM). The three alternatives serve the same AtomicAssets state
query surface; history (`/logs`, transfers, sales-history) is out of the state tier (→ Elasticsearch).

- **eosio-contract-api** — the incumbent: decoded state in PostgreSQL (live WAX node, catalog stats).
- **snapshot-load → MongoDB** — Track A: the Hyperion-4.5 state tier (decode a snapshot → Mongo).
- **WormDB `.wseg`** — Track B: the compiled, mmap'd faceted store (this POC: `crates/wseg-build`).

## The matrix (WAX, 232.3M live assets unless noted)

| | eosio-contract-api (Postgres) | snapshot-load → Mongo | WormDB `.wseg` (POC) |
|---|---|---|---|
| **assets storage on-disk** | **211 GB** (458M rows, incl. burned) | **23.3 GB** (232M live) | **22.7 GB** (232M, measured) |
| **full atomic state on-disk** | **~692 GB** (DB total 1.27 TB) | **24.3 GB** | 22.7 GB (assets+defs; market TBD)¹ |
| **resident memory to serve** | many GB² (shared_buffers + page cache) | **15.3 GB** (mongod RSS) | **5 MB cold / 22 MB hot** (heaviest query) |
| **bootstrap time** | days (SHiP action replay) | **38 min** (snapshot → indexed) | **5.9 min @ 88.8M / 17.7 min @ 232M** (from Mongo) |
| **point lookup (asset + join)** | ~ms | **0.75 ms** | **0.1 µs** |
| **owner / collection page (100)** | ~ms | **0.8 ms** | **<0.1 µs** |
| **faceted intersect (`data:rarity`)** | ~ms (GIN) | **0.76 ms** | **0.95 ms** (raw merge; roaring → µs)³ |
| **browse (sorted page)** | ~ms | **1.2 ms** | **<0.1 µs** |
| **architecture** | client ↔ server (TCP) | client ↔ server (TCP) | **embedded (in-process mmap)** |
| **history (logs/transfers/sales)** | in-DB (drives the 692 GB) | → Elasticsearch | → Elasticsearch |

¹ The POC segment holds atomicassets only (assets/templates/schemas/collections defs); AtomicMarket is
not yet in the segment.
² PostgreSQL RSS wasn't isolated; its 126 GB of assets indexes must be OS-page-cached for fast queries,
so the effective resident working set is many GB.
³ Point/page/browse are O(log N)/O(1) → sub-µs at any scale. The faceted **intersect** is a linear
sorted-merge of two posting lists, so it scales with posting size: ~11 µs intersecting a 1.7M-asset
collection @ 88.8M, but **0.95 ms** intersecting a 963k-asset collection ∩ a 128k-asset rarity posting
@ 232M (a ~1M-element merge) — comparable to Mongo's GIN. This is exactly the case **roaring bitmaps**
collapse back to µs (compressed bitmap-AND ≈ O(containers), not O(elements)) — the #1 Phase-2 item.

## The honest read

- **vs Postgres — dramatic on every axis.** ~28× less storage (state-only, no burned rows, no history),
  minutes vs days to bootstrap, MB vs GB resident. Most of their 692 GB is *history* Hyperion already
  holds in ES + indexes over burned rows.
- **vs Mongo — the win is resident memory + latency, not on-disk size.** The POC's raw-`u64` posting
  lists make the segment *comparable* on disk to WiredTiger-compressed Mongo (~8.5 GB @ 88.8M either
  way). What changes is **how it serves**: mmap faults only the touched blobs, so the process stays
  **6 MB resident regardless of an 8.5 GB segment**, vs mongod holding **15.3 GB** of warm index pages.
  And queries are **in-process µs** vs Mongo's **~ms** (which includes the client↔server round-trip).
- **Latency caveat:** WormDB is *embedded* (no IPC); Mongo/PG numbers include the localhost round-trip.
  The architecturally-fair comparison is the resident-memory + embedded-serving model, not raw ns vs ms.

## What the POC covers (and doesn't, yet)

Measured POC = forward store (asset + template, normalized) + inverted indexes
(owner / collection / schema / template / `data:rarity`) + presorted `sorted_id`. Reader = `aa-probe`
(mmap + Q1–Q5). It is **read-only / frozen** and indexes one data facet (`rarity`).

Not yet (the path to full parity + the on-disk win):
- **Roaring / delta-encoded postings** — the ~3.4 GB of raw-`u64` posting lists compress heavily
  (sorted ids delta+varint), which is what takes WormDB *below* Mongo on disk too.
- **Full `data:*` attribute coverage** (all facets) + numeric range indexes (price, template_mint).
- **Freshness overlay** (frozen segment + SHiP-fed delta) for live updates.
- **AtomicMarket** tables + the WormDB Zig reader serving the HTTP API.

## How to reproduce

```sh
# build the segment from the Mongo state snapshot-load wrote
cargo run --release -p wseg-build --bin aa-build -- --db aatest_waxtest --out aa.wseg
# benchmark it (size breakdown + Q1-Q5 latency + resident RSS)
cargo run --release -p wseg-build --bin aa-probe -- --seg aa.wseg --iters 5000
# the Mongo baseline for the same queries
node benchmark/atomicassets/validate/mongo-bench.js aamain_wax 200
```

### Raw POC measurements

| segment | assets | on-disk | build | RSS cold→hot | Q1 | Q3 intersect |
|---|---|---|---|---|---|---|
| testnet 5M | 5,000,000 | 674 MiB | 24 s | 5.2 → 5.9 MB | 300 ns | 11.2 µs |
| testnet full | 88,844,041 | 8.5 GiB | 5.9 min | 5.0 → 6.0 MB | 100 ns | 11.5 µs |
| mainnet | 232,303,581 | 22.7 GiB | 17.7 min | 5.0 → 26.9 MB | 100 ns | 0.95 ms |

Mainnet segment breakdown: asset forward store 13.2 GB (4.4 GB index + 8.8 GB blobs); the four inverted
posting lists + `sorted_id` ~1.8 GB each (= 232M × 8 B raw `u64`, the part roaring/delta compresses);
`by_owner` has 15.2M distinct owners; templates 253 MB.
