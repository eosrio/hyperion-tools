# AtomicAssets API on hyperion-tools — architecture & requirements

> How an operator serves the **AtomicAssets API at full parity** as part of *one* stack that also
> serves **Hyperion v2 + Light API + native chain v1** — storing each datum **once**, in the tier
> matched to its access pattern, adding **no new database**. AtomicAssets is the proving case (the
> richest query surface); the same machinery generalizes to every Antelope API shape.

## 1. Constraints (non-negotiable)

- **All Antelope API shapes, minimal storage waste** — one stack for Hyperion v2, Light API, AtomicAssets, chain v1.
- **No extra DB stack.** Reuse only what the operator already runs: **Elasticsearch** (history), **archive-server** (cold), **WormDB** (state / fast in-db apps), **nodeos** (chain). Nothing new — no Postgres, Mongo, Redis, or a bolted-on OLAP engine.
- **Full parity, or it's pointless.** Partial parity ⇒ operators keep running eosio-contract-api (and its ~1.1 TB Postgres) for the gaps ⇒ **zero** storage reclaimed. Only full parity lets them delete it. Full parity is therefore a *storage* requirement, not a feature wish.

## 2. The waste we delete

eosio-contract-api on WAX = a **second SHiP reader** + **~1.1 TB PostgreSQL** + Redis (64 GB RAM, months to fill). Most of that 1.1 TB is **history — transfers, mints, sales and their indexes — data Hyperion already holds in Elasticsearch.** It is stored twice and read twice. The goal is not a smaller Postgres; it is **don't store or read it twice at all.**

## 3. Architecture: one read, three tiers, each datum once

```
   snapshot ─(bootstrap state)─┐
                               ▼
            ┌──────── hyperion-ship: ONE direct-from-disk reader/decoder ────────┐
            └──────────────────────────────┬───────────────────────────────────-─┘
                       projects each datum to exactly ONE tier
   ┌───────────────────────┬───────────────┴───────────────┬───────────────────────┐
   ▼                       ▼                                ▼                       ▼
 WormDB  (STATE)     Elasticsearch (HISTORY)        archive-server (COLD)      nodeos (CHAIN)
 mmap columnar +     actions/deltas, searchable     frozen SHiP logs,          push / live
 inverted indexes;   — Hyperion v2 already runs it   on-demand hydration
 current contract                                    of old payloads
 tables, all apps
   └───────────────────────┴───────────────┬───────────────┴───────────────────────┘
                                            ▼
        Unified API layer — routes each endpoint to the tier that owns its data
       (Light API · AtomicAssets · Hyperion v2 · chain v1 — all parity-faithful)
```

- **State → WormDB.** Current contract-table state for *every* contract (system, tokens, atomicassets, atomicmarket, any app), as a compact mmap columnar store with purpose-built indexes; mutated via a live overlay (§5).
- **History → Elasticsearch.** Actions + deltas. Hyperion v2 already indexes the whole chain here — including every `atomicassets`/`atomicmarket` action — so AtomicAssets activity/logs/transfers/sales-history are *queries over data ES already holds.*
- **Cold → archive-server.** Old `act.data` / `contract_row` payloads straight from frozen logs, no DB.
- One **shared reader**; **snapshot-load** bootstraps WormDB state in seconds.

No tier re-stores another's data. That single rule *is* the storage-efficiency story.

## 4. The hard part: WormDB as a *compiled* faceted-query engine for state

Light-API state = single-key lookups → the u64 `.wseg` segment is perfect. AtomicAssets state =
**faceted search**: filter on many fields *including decoded attributes*, sort, paginate, join,
aggregate. The segment must grow a query layer — but a **bounded, compiled** one (the API surface is
fixed at ~40 endpoints), not a general SQL engine. WormDB adds:

1. **Columnar forward store** per object (assets / templates / schemas / collections / offers / market listings): parallel typed columns + a primary u64 index for point lookups and joins.
2. **Inverted indexes (postings)** per filter dimension — `owner`, `collection`, `schema`, `template`, and each queryable **data attribute** (`collection:schema:attr → value`): sorted / roaring u64 posting lists. Multi-filter = **intersect** postings; numeric attrs and prices = **range** over a value-sorted index.
3. **A few sort orderings** (`asset_id`, `minted`, `updated`, `transferred`, market `price`) as presorted id arrays; merge with the filter intersection, then skip+take for pagination.
4. **In-process assembly** — resolve an asset's template/schema/collection and merge immutable+mutable `data` at request time from the columnar store (a *join*, not stored denormalized).
5. **Overlay freshness** — the frozen segment (from snapshot) + a delta overlay the SHiP feed maintains for recent mints/transfers/burns/listings, merged at query time; periodic re-segment bounds the overlay. (The Light-API overlay model, extended to the indexes.)

This is a compiled mini-search-engine — "compile the API into the DB," taken to the query-heavy case.

### Why this is *more* storage-efficient than the 1.1 TB Postgres
- **Normalize, join at request time.** Most assets share a template's `immutable_data`; store the template's data **once** and let each asset carry only `(template_id, owner, mutable_data)`. eosio-contract-api denormalizes per-asset and indexes it → bloat. In-process joins on mmap'd columns are cheap.
- **Columnar, no row/MVCC/WAL bloat**; postings delta-varint / roaring compressed.
- **History isn't here** — it's in the ES the operator already runs. WormDB holds only live state.
- Working hypothesis to validate in step 3: WAX AtomicAssets *current state* ≈ **tens of GB columnar** vs **~1.1 TB** Postgres (state + history + indexes + bloat).

### The gateway gap
`*_serialized_data` is a **custom schema-driven binary format (`eosio::atomicdata`), not ABI** — `abieos` can't touch it, so today those fields are opaque hex. A Rust decoder driven by `schemas.format` (port of atomicassets-js / the contract's `eosio::atomicdata`) is the prerequisite for any attribute column or index. Bounded, deterministic — **build it first.**

## 5. Endpoint → tier parity map (full coverage)

| endpoint group | tier | notes |
|---|---|---|
| `assets`, `templates`, `schemas`, `collections`, `offers` — list/filter/sort/paginate + `data:*` | **WormDB** | the faceted-query engine (§4) |
| `…/{id}` point reads, `accounts/*` (asset counts per owner/collection) | **WormDB** | point lookup + join; materialized counters |
| `…/{id}/logs`, `transfers`, `burns` | **ES** | history Hyperion already indexes; an AA-aware query/transform layer |
| `atomicmarket/sales|auctions|buyoffers` — **active** listings (state filter) | **WormDB** | current listings + price-sorted index for range/sort |
| `…/sales|auctions` — **historical** (sold/cancelled/expired) | **ES** | completed events |
| `prices`, `stats/*`, `…/stats`, price/volume graphs, suggested-median | **ES + WormDB** | API layer merges: ES time-series aggregation ⊕ WormDB floor/counts |
| `marketplaces`, `config` | **WormDB** | small static-ish state |

Every endpoint maps to a tier; the few that **span** tiers (sales-by-state, prices, stats) are
orchestrated by the unified API layer (WormDB state ⊕ ES history). That orchestration is the price of
full parity — and full parity is what lets eosio-contract-api be deleted.

## 6. Generalizes to all Antelope API shapes

The engine §4 builds for AtomicAssets is the same one that serves the rest:

- **chain v1 reads** — `get_table_rows`, `get_account`, `get_currency_balance`, secondary-index ranges → **WormDB** state (same columnar + index machinery). Push/live → nodeos.
- **Hyperion v2** — `get_actions`/`get_deltas` → **ES** + **archive-server** (cold); state helpers (`get_voters`, `get_proposals`, `get_tokens`, `get_key_accounts`) → **WormDB**.
- **Light API** → **WormDB** (done).
- **AtomicAssets** → **WormDB** (state+market) + **ES** (history) + **archive** (cold).

AtomicAssets is simply the hardest instance; solving it gives WormDB the faceted-query capability the others reuse.

## 7. Build sequence (target = full parity)

1. **`atomicdata` decoder** + schema-format registry; validate by diffing decoded `data` vs a live AtomicAssets API. *Gateway — do first.*
2. **WormDB faceted-query core** — columnar store + inverted/range indexes + intersect/sort/paginate; prove on `/assets` (the worst case).
3. **State bootstrap** — `snapshot-load --tables atomicassets,atomicmarket` → decoded segment; **measure the state footprint** (first efficiency datapoint).
4. **Remaining state/market read endpoints** + in-process assembly (joins, data merge).
5. **History endpoints as an ES projection layer** — parity-faithful `/logs`, `/transfers`, sales-history over the actions Hyperion already indexes.
6. **Market aggregation** + delphioracle; the tier-spanning endpoints (§5).
7. **Live overlay + index maintenance** from the SHiP feed; re-segment cadence.
8. **Benchmark** vs eosio-contract-api as `benchmark/atomicassets/`: parity + throughput + the headline **hardware/storage footprint**.

Full parity is the bar; the phases are build order, not a place to stop.

## 8. Open design questions (the real ones now)

- **Index maintenance under writes** — frozen postings + overlay delta-index merged at query time, vs an LSM; re-segment cadence vs overlay growth.
- **Attribute index encoding** — dictionary + roaring per `(collection, schema, attr)`; bounding cardinality across thousands of schemas.
- **History parity via ES** — reproducing eosio-contract-api's exact `/logs` and market-stats shapes from Hyperion's generic action docs (query/transform layer, possibly a thin AA-aware ES projection).
- **Tier-spanning endpoints** — the API layer's WormDB-state ⊕ ES-history merge for `sales`(by state), `prices`, `stats`.
- **Standalone profile** — an AtomicAssets-only operator has no Hyperion ES; do they get history from archive-server + a WormDB activity index, or is the all-APIs operator the only target?
