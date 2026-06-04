# AtomicAssets API on Hyperion — architecture & delivery plan (4.5)

> **Target: Hyperion 4.5, for existing Hyperion operators.** Serve the **AtomicAssets API at full
> parity** as part of the stack that already serves Hyperion v2 + Light API + native chain v1 — adding
> **no new infrastructure**, storing each datum **once**. AtomicAssets is the proving case (the richest
> query surface); the same machinery generalizes to every Antelope API shape.

## 1. The operator already runs every tier

A Hyperion 4.5 operator already has the whole tiered stack — AtomicAssets serving is **pure software on
top of it, no new database**:

| tier | already running | role |
|---|---|---|
| **history** | **Elasticsearch** | actions + deltas for the whole chain — incl. every `atomicassets`/`atomicmarket` action |
| **state** | **MongoDB** (since Hyperion v4.0, for state queries) | current contract-table state |
| **cold** | **archive-server** (new in 4.5) | old `act.data` / `contract_row` payloads from frozen logs |
| **chain** | **nodeos** | push / live |

The operator **deletes their separate eosio-contract-api PostgreSQL (~1.1 TB) + filler + Redis** (and
the cc32d9 MariaDB for Light API) and serves everything from the Hyperion infra they already run.

## 2. Why full parity is the storage win (not a feature wish)

Partial parity reclaims **zero** storage: operators keep eosio-contract-api (and its ~1.1 TB) running
for the missing endpoints, so they pay for both stacks. Only **full parity** lets them delete it. And
most of that 1.1 TB is **history — transfers/mints/sales + indexes — which Hyperion already holds in
Elasticsearch**, stored twice and read twice. The win is *delete the duplicate*, not shrink it.

## 3. Each datum once (no duplication)

- **State → MongoDB** — assets / templates / schemas / collections / offers / market listings, **decoded**.
- **History → Elasticsearch** — already there; `/logs`, `/transfers`, sales-history are *queries over it*.
- **Cold → archive-server** — old payloads straight from frozen logs.
- One shared reader: Hyperion's SHiP indexer keeps state current; `snapshot-load` bootstraps it fast.

History is never re-stored in the state tier; state is never re-stored in history. That rule *is* the
storage-efficiency story.

## 4. Two-track delivery (the strategy)

The **state tier** has two implementations behind **one substrate-agnostic API** — so 4.5 ships now and
the efficiency endgame lands later without re-integration or operator disruption:

- **Track A — MongoDB · ships in 4.5.** Leverage the MongoDB Hyperion already runs. Mongo serves the
  AtomicAssets query surface natively (compound indexes, range, sort, pagination, aggregation
  pipeline), and `snapshot-load` already has a high-throughput Mongo sink. Lowest-risk path to full
  parity on the existing stack.
- **Track B — WormDB · in parallel, the efficiency endgame.** Extend WormDB into a compiled
  faceted-query engine (§6) — mmap columnar + inverted indexes, tens-of-MiB resident — and swap it
  behind the same API when proven. The ultimate hardware-efficiency state tier.

The API server talks to a **state-store interface**, not to Mongo directly, so Track B drops in behind
it transparently. The `atomicdata` decoder (§5) and the API/parity work are **shared** by both tracks.

## 5. The gateway gap (shared by both tracks)

`*_serialized_data` is a **custom schema-driven binary format (`eosio::atomicdata`), not ABI** —
`abieos` can't touch it, so today those fields are opaque hex. A Rust decoder driven by the
`schemas.format` (port of atomicassets-js / the contract's `eosio::atomicdata`) is the prerequisite for
storing/indexing decoded attributes in *either* store. Bounded, deterministic — **build it first.**

## 6. Track B: WormDB as a compiled faceted-query engine

The Light-API `.wseg` segment is single-key (perfect for "everything about account X"). AtomicAssets is
**faceted search** (filter on many fields incl. decoded attributes, sort, paginate, join, aggregate),
so WormDB grows a **bounded, compiled** query layer (the API is ~40 fixed endpoints, not arbitrary SQL):

1. **Columnar forward store** per object + a primary u64 index for point lookups/joins.
2. **Inverted indexes (postings)** per filter dimension — owner, collection, schema, template, each queryable **data attribute** — sorted/roaring u64 lists; multi-filter = **intersect**, numeric/price = **range** over a value-sorted index.
3. **A few presorted orderings** (asset_id, minted, updated, transferred, price); merge with the filter set; skip+take.
4. **In-process assembly** — resolve template/schema/collection and merge immutable+mutable `data` at request time (a *join*, not stored denormalized).
5. **Overlay freshness** — frozen segment + SHiP-fed delta overlay, periodic re-segment.

**Why WormDB beats both Postgres and a Mongo state store on footprint:** normalize and join at request
time (store a template's `immutable_data` **once**; assets carry only `template_id`+`owner`+mutable);
columnar with no row/MVCC/WAL bloat; history not co-located. Hypothesis to measure: WAX AtomicAssets
*state* ≈ tens of GB (Postgres) → far smaller columnar, tens-of-MiB resident.

## 7. Endpoint → tier parity map (full coverage)

| endpoint group | tier | notes |
|---|---|---|
| `assets`,`templates`,`schemas`,`collections`,`offers` — list/filter/sort/page + `data:*` | **State** (Mongo→WormDB) | the faceted query surface |
| `…/{id}` point reads, `accounts/*` counts, `marketplaces`,`config` | **State** | point lookup + join; materialized counts |
| `…/{id}/logs`, `transfers`, `burns` | **ES** | history Hyperion already indexes (query/transform layer) |
| `atomicmarket/sales\|auctions\|buyoffers` — **active** (state filter) | **State** | current listings + price index |
| `…/sales\|auctions` — **historical** (sold/cancelled/expired) | **ES** | completed events |
| `prices`, `stats/*`, graphs, suggested-median | **ES + State** | API layer merges ES time-series ⊕ state floor/counts |

Every endpoint maps to a tier; the few that **span** tiers (sales-by-state, prices, stats) are
orchestrated by the API layer. Full coverage is the bar — that's what lets eosio-contract-api be deleted.

## 8. Generalizes to all Antelope API shapes

Same machinery: **chain v1 reads** (`get_table_rows`/`get_account`/secondary ranges) → state tier;
**Hyperion v2** (`get_actions`/`get_deltas`) → ES + archive, state helpers → state tier; **Light API** →
state tier (already on Mongo via the `light-api` crate, and on WormDB via `.wseg`). AtomicAssets is just
the hardest instance — solving it gives the state tier the faceted-query capability the others reuse.

## 9. Build sequence

**Track A (4.5):**
1. ✅ **`atomicdata` decoder** (`crates/atomicdata`) — schema-format-driven, byte-exact (spec cross-validated across the contract C++, atomicassets-js, and XPRNetwork). Validated against the golden on-chain vector + **77/77 distinct-schema live WAX templates** (output matches eosio-contract-api's stored `immutable_data` exactly). *Done.*
2. 🟡 **`snapshot-load --tables atomicassets,atomicmarket`** → decode → Mongo collections + AA-specific indexes. *Core done:* the `atomicassets`/`atomicmarket`/`atomic` preset (seek path) decodes the live state — `schemas`/`collections`/`templates`/`assets`/`offers`/`config` and `sales`/`auctions`/`buyoffers`/`tbuyoffers`/`marketplaces`/`config` — into eosio-contract-api-shaped Mongo collections (`atomicassets-*`/`atomicmarket-*`), resolving every `serialized_data` blob via a two-pass **schema-format registry** (a pre-pass over `schemas` + the global `config.collection_format`) through the `atomicdata` crate. `assets`/`templates`/`collections` carry decoded `immutable_data`/`mutable_data` + a merged `data` (mutable-wins) with a `data.$**` wildcard index; market price/symbol fields are parsed to exact base units. **S+D fields only — history-derived (H) fields (mint/transfer/burn timestamps, `template_mint`, bid history, terminal-state rows) are deferred to the feed/ES.** *Validated on WAX testnet* (block 409250749, 2026-06-04): **89.6M docs decoded with 0 errors**; a random sample diffed against the live `test.wax.api.atomicassets.io` gives **150/150 immutable_data + 60/60 mutable_data + 150/150 structural exact matches** (the one parity fix it surfaced: asset `float`/`double` attributes render as strings on the live API while templates keep numbers — handled in `map_asset`). State footprint incl. all indexes: **31.3 GB** for 88.8M assets (≈352 B/asset; the `data.$**` wildcard index is 5.5 GB and dominates the post-load build). Harness: `benchmark/atomicassets/validate/`. *Remaining:* run on a **WAX mainnet** snapshot for the headline footprint number vs eosio-contract-api's ~692 GB.
3. AtomicAssets API server over the state-store interface: state→Mongo, history→ES, cold→archive — **full endpoint parity**.
4. Live freshness via Hyperion's existing delta→Mongo pipeline + the decoder (confirm how much it already indexes — §10).
5. Market aggregation (Mongo pipeline + ES) + delphioracle; the tier-spanning endpoints.
6. Benchmark vs eosio-contract-api: parity + throughput + **hardware/storage footprint**, as `benchmark/atomicassets/`.

**Track B (parallel):** WormDB faceted-query engine (§6), shared decoder; swap behind the API; re-benchmark the state footprint (tens-of-MiB-resident target).

## Live deployment findings (WAX, eosio-contract-api v1.3.24)

Studied a live deployment (Postgres `api-wax-mainnet-atomic-1` + the public API) to ground Track A:

- **It stores decoded data, as JSONB.** `atomicassets_assets.{immutable_data,mutable_data}` and
  `atomicassets_templates.immutable_data` are `jsonb` (the resolved attributes); `schemas.format` is a
  `text[]` of `{name,type}`. So eosio-contract-api decodes (via atomicassets-js) at fill time and stores
  the resolved data — Track A mirrors this: decode with `atomicdata` → Mongo docs carrying resolved data.
- **It materializes purpose-built filter/aggregate tables, not just raw indexes:**
  `atomicmarket_sales_filters_{listed,sold,waiting,updates,invalid}`, `atomicassets_asset_counts`,
  `atomicmarket_template_prices`, `atomicmarket_stats_markets`. These are exactly the indexes /
  materialized views Track A must build on Mongo (and Track B in WormDB) for the hot endpoints (sales,
  account/collection counts, floor prices).
- **Sizing confirms the thesis.** Rows: assets **468 M** (incl. burned), offers **180 M**, sales
  **172 M** — vs definitions templates **904 K**, schemas **80 K**, collections **110 K**. Total DB
  **1.27 TB**; the atomicassets+atomicmarket tables **692 GB** — *dominated by lifecycle history* (burned
  assets, completed/cancelled offers + sales), which Hyperion already holds as actions in ES. The compact
  *live* state (definitions + non-burned assets + active listings) is a fraction → storing only that,
  with history served from the existing ES, is the storage win.

## 10. Open questions

- **What does Hyperion 4.0+ Mongo already index for atomicassets?** If table deltas already land (raw), Track A is mostly *decode + AA-shaped projection + API* — confirm the existing state pipeline.
- **Mongo state footprint for WAX AA** — measure early; it's the headline 4.5 efficiency datapoint.
- **History parity via ES** — reproducing eosio-contract-api's exact `/logs` and market-stats shapes from Hyperion's generic action docs (query/transform, maybe a thin AA-aware ES projection).
- **Scope of v1 contracts** — atomicmarket + atomictools + delphioracle are entangled (market needs delphi prices); full parity needs all, sequence atomicassets-core → market.
- **Standalone (non-Hyperion) AA operator** — out of scope for 4.5 (this targets existing Hyperion operators); revisit later.
