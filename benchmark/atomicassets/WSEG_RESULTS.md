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
| **owner / collection page (100)** | ~ms | **0.8 ms** | **0.1 µs** |
| **faceted filter (`collection,data:rarity`)** | ~ms (GIN) | **0.76 ms** | **0.1 µs** (single posting)³ |
| **browse (sorted page)** | ~ms | **1.2 ms** | **<0.1 µs** |
| **multi-attr intersect (worst case)** | ~ms (GIN) | ~ms | 1.2 ms raw / **7.5 µs** roaring⁴ |
| **architecture** | client ↔ server (TCP) | client ↔ server (TCP) | **embedded (in-process mmap)** |
| **history (logs/transfers/sales)** | in-DB (drives the 692 GB) | → Elasticsearch | → Elasticsearch |

¹ The POC segment holds atomicassets only (assets/templates/schemas/collections defs); AtomicMarket is
not yet in the segment.
² PostgreSQL RSS wasn't isolated; its 126 GB of assets indexes must be OS-page-cached for fast queries,
so the effective resident working set is many GB.
³ `data_attr_key` is keyed by `(collection, schema, field, value)`, so a `collection,data:rarity=X`
filter is a **single posting lookup + page slice** — O(log N) + O(100), sub-µs at any scale. A true
*intersect* is only needed for a multi-attribute AND (two different filter postings).

⁴ Measured roaring experiment (mainnet, `aa-probe`): the genuine 963k ∩ 128k multi-attr AND is **1.16 ms**
as a raw sorted-u64 merge vs **0.48 ms** roaring (deserialize + AND) vs **7.5 µs** roaring AND on resident
bitmaps (skips non-overlapping high-32 buckets). Roaring's intersect speed depends on overlap (fast when
the two sets live in different asset_id buckets, e.g. a specific rarity ∩ a collection — measured
roaring deser+AND **20 µs** for 1.7M ∩ 8888 @ 88.8M, **0.48 ms** for 963k ∩ 128k @ 232M). **Roaring
also compresses the posting lists 3.7–56× on this data** (e.g. 13 MB raw → 0.24 MB) — that is the real
Phase-2 on-disk win: the ~9 GB of raw `u64` posting tables → ~1–2 GB, dropping the segment well under
Mongo's 23 GB. Open tradeoff: page queries want a zero-copy sorted tail, the intersect wants the bitmap
— so the posting format is likely a hybrid (raw for page dimensions, roaring/block-delta for facets).

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

| segment | assets | on-disk | build | RSS cold→hot | Q1–Q5 (point/page/facet/browse) | multi-attr intersect (raw → roaring) |
|---|---|---|---|---|---|---|
| testnet 5M | 5,000,000 | 674 MiB | 24 s | 5.2 → 5.9 MB | ≤0.3 µs | — |
| testnet full | 88,844,041 | 8.5 GiB | 5.9 min | 5.0 → 6.0 MB | ≤0.1 µs | 1.85 ms → 20 µs (56× smaller) |
| mainnet | 232,303,581 | 22.7 GiB | 17.7 min | 5.0 → 26.9 MB | ≤0.1 µs | 1.16 ms → 0.48 ms (18× smaller) |

Mainnet segment breakdown: asset forward store 13.2 GB (4.4 GB index + 8.8 GB blobs); the four inverted
posting lists + `sorted_id` ~1.8 GB each (= 232M × 8 B raw `u64`, the part roaring/delta compresses);
`by_owner` has 15.2M distinct owners; templates 253 MB.
