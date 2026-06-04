# AtomicAssets API on hyperion-tools — requirements & mapping

> Research note. Goal: serve the **AtomicAssets API** (the pinknetworkx/eosio-contract-api shape) as
> part of the larger objective — **one operator serving Hyperion v2 + Light API + AtomicAssets +
> native chain v1 off shared infrastructure**, at a fraction of the hardware the four reference stacks
> need today. AtomicAssets is the hardest of the four to fit, because it is *state + history + market
> aggregation* behind a rich query API — not the single-key state lookups the Light API is.

## 1. What AtomicAssets costs today (the target to beat)

Reference stack (eosio-contract-api / pink.gg), per the [WAX operator guide](https://docs.wax.io/operate/atomic-assets/setup-wax-atomic-api-node):

- nodeos (`state_history_plugin`) + the **filler** (its own SHiP reader) + **PostgreSQL ≥17** + **Redis** + a Node API.
- WAX hardware: **8-core 5 GHz+, 64 GB+ RAM, 2 TB+ enterprise NVMe**; PostgreSQL DB **≈1.1 TB** (Mar 2025).
- Initial fill from chain: **months** for WAX mainnet — operators restore from guild DB backups instead.
- Operationally fragile (a double-started filler corrupts the DB).

So every AtomicAssets operator runs a **second full SHiP reader and a second TB-class database**, even when they already run Hyperion (Elasticsearch) and/or a Light API. That duplication is the efficiency target.

## 2. The unified efficiency thesis

One **direct-from-disk SHiP read+decode core** (`crates/core` = `hyperion-ship`; core-scaling, no nodeos `ship`-serializer bottleneck) decodes the chain **once** and projects into whatever per-API store each surface needs; `snapshot-load` bootstraps current **state** in seconds, not months. The reference stacks each re-read and re-store the same chain independently.

Where each API's data already is in a Hyperion deployment:

- **chain v1** → nodeos itself.
- **Light API** (state) → the mmap `.wseg` segment / Mongo (done — see [`../lightapi`](../lightapi)).
- **AtomicAssets history** (transfers, mints, burns, sale/auction events, per-object logs) → **already in Hyperion's action index**: `atomicassets::{logmint,logtransfer,lognewtempl,logburn,logbackasset,logsetdata,…}` and `atomicmarket::{lognewsale,…}` are ordinary ABI-decodable action traces.
- **AtomicAssets state** (assets/templates/schemas/collections/offers + market listings) → contract-table state (snapshot + SHiP deltas) — **once the AtomicAssets payload is decoded** (see §5.1).

## 3. The API surface — why it's the hard case

The Light API is current-state **single-key lookups** ("everything about account X"; "top-N holders of token T") — a perfect fit for the u64-keyed mmap segment. AtomicAssets is a **query engine**.

~40 endpoints across `/atomicassets/v1` (`assets`, `collections`, `schemas`, `templates`, `offers`,
`transfers`, `accounts`, `burns`, `config`, plus per-object `/logs` and `/stats`) and `/atomicmarket/v1`
(`sales`, `auctions`, `buyoffers`, `marketplaces`, `prices`, `stats`). The `assets` endpoint alone:

- filters by **collection, schema, template, owner — and decoded data attributes** (`data:rarity=Legendary`, ranges like `template_mint`/`min_template_mint`),
- **sorts** by arbitrary fields (`asset_id`, `minted`, `transferred`, `updated`, `template_mint`, market `price`),
- **paginates** (`page`/`limit`, cursor),
- **joins** asset → template → schema → collection to resolve display data,
- and `/stats` + `/prices` need **aggregation** (floor, volume, median, counts).

→ multi-dimensional indexes, range scans, multi-field sort, pagination, joins, group-by aggregation.
**The single-key `.wseg` segment serves none of these** (one index dimension, exact-match only). This is
the key structural finding: the Light-API segment model does **not** extend to AtomicAssets.

## 4. The data, decomposed

| layer | examples | source in hyperion-tools | decode boundary |
|---|---|---|---|
| **State** | assets, templates, schemas, collections, offers, balances; market sales/auctions/buyoffers/config | `snapshot-load` (bootstrap) + `delta-proto` / SHiP feed (fresh) | ABI decodes the row, but the `*_serialized_data` blobs come out as **raw hex** — need the AA decoder |
| **History / activity** | transfers, mints, burns, attribute changes, sale/auction events, per-object `/logs` | `action-proto` (action traces) + `delta-proto` (row deltas) — **already what Hyperion indexes** | fully ABI-decodable (the `log*` actions carry structured fields) |
| **Derived / market** | floor price, 24 h volume, collection/account stats, suggested median, price history | computed over state + history (+ delphioracle) | aggregation logic (new) |

## 5. Reusable vs new

### Reusable (the shared backbone)
- **`crates/core` (hyperion-ship)** — zero-copy SHiP/disk trace + delta parsers; the one reader for every API.
- **`snapshot-load`** — framing + ABI decode of **any** contract table (`--tables atomicassets:assets` / `atomicmarket:sales` / `*`); the `--mongo` sink (>600 K docs/s) and the dynamic `@`-doc shape (`map_dynamic` → `atomicassets-assets`, …).
- **`action-proto` / `delta-proto`** — AA action logs + table-row deltas; the `@`-field handler dispatch is the extension point for AA actions.
- **The snapshot → serve → SHiP-feed freshness model** proven on the Light API.

### New, AtomicAssets-specific (the gaps)
1. **AtomicAssets `serialized_data` decoder — the central gap.** A *custom schema-driven binary format* (`eosio::atomicdata`), **not** Antelope ABI — `abieos`/`rs_abieos` cannot touch it, so today those fields are opaque hex everywhere. Decode = read the collection's `schemas.format` (list of `{name, type}`: int/uint/fixed 8–64, `bool`, `bytes`, `string`, `ipfs`, `float`, `double`, and `[]` arrays) and walk the blob. Port from [`atomicassets-js`](https://github.com/pinknetworkx/atomicassets-js) / the atomicassets-contract. Bounded and deterministic — but it **blocks everything attribute-related**.
2. **A query substrate + indexes** for attribute filters, joins, sort, pagination, aggregation (§6).
3. **Market aggregation** — floor/volume/stats/median, delphioracle price pairs.
4. **History → activity projection** — per-asset/-template/-collection `/logs`, assembled by entity from the action/delta stream.
5. **AA `@`-field action handlers** + offer/listing **lifecycle state** (listed → sold/cancelled/expired).
6. **The AtomicAssets HTTP API server** — the ~40 routes with eosio-contract-api's query semantics.

## 6. The one real decision: the query substrate

The segment model doesn't fit, so this is the pivotal choice — evaluated through the efficiency lens:

| option | fit for the query surface | hardware-efficiency story |
|---|---|---|
| **Reuse the operator's Elasticsearch** (Hyperion already runs it) | history is already there; state + decoded attributes index naturally; rich filter/sort/aggregation/full-text out of the box | **strongest for Hyperion operators — no new database.** Needs AA→ES projections + an API layer over ES |
| **Embedded OLAP / columnar** (DuckDB, or columnar + Parquet) | mmap, columnar, joins + aggregation, no separate DB process — closest to the WormDB ethos | best **standalone** profile (lightweight AA serving without a full Hyperion); a new engine to integrate |
| **MongoDB** (existing `--mongo` sink) | compound indexes + aggregation pipeline; least new ingest code | adds a DB process; weaker on full-text + analytics than ES/DuckDB |
| **Extend WormDB into a columnar / secondary-index store** | ultimate single-engine unification | largest build — effectively a new query engine + planner |

**Recommendation to evaluate first:** *ES-reuse* for operators already running Hyperion (zero extra
database; leans on the existing ES + the shared reader), and *embedded columnar (DuckDB)* for the
standalone/lightweight profile. Both keep the core win: **read the chain once, no second TB database.**
(PostgreSQL is the reference; we'd match it only to be a drop-in, not to win on hardware.)

## 7. Proposed sequence (research → benchmark)

1. **Decoder first.** Rust `atomicdata` (de)serializer + a schema-format registry; validate by decoding real WAX assets and diffing `data` against a live AtomicAssets API. Unblocks everything.
2. **State bootstrap.** `snapshot-load --tables atomicassets,atomicmarket` → run blobs through the decoder → typed docs (assets/templates/schemas/collections/offers/market). **Measure the state size** — current state is far smaller than 1.1 TB (that figure is mostly history + indexes); this is the first efficiency datapoint.
3. **Pick the substrate** (§6) and build the index/query layer + the core read routes (`assets`, `templates`, `collections`, `schemas`, `sales`).
4. **History / activity** from the shared reader (or the operator's existing ES) for `/logs`, `/transfers`, sale history.
5. **Market aggregation** + delphioracle.
6. **Benchmark** vs eosio-contract-api as `benchmark/atomicassets/` (mirroring `benchmark/lightapi/`): response **parity**, **throughput**, and the headline **hardware footprint** — sync time, RAM, disk.

## 8. Open questions

- **Substrate:** ES-reuse (Hyperion operators), embedded-columnar (standalone), or support both profiles?
- **Parity scope:** full ~40-endpoint drop-in, or the high-traffic subset (`assets`/`templates`/`collections`/`sales`) first?
- **History source:** lean on the operator's existing Hyperion ES, or project a self-contained AA activity store?
- **Scope:** atomicmarket + atomictools + delphioracle in v1, or atomicassets-only first?
