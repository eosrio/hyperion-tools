# AtomicAssets snapshot-load validation

Two small Node scripts to validate a `snapshot-load --tables atomic` Mongo load: the **state
footprint** and **decode correctness** vs the live AtomicAssets API.

## Load a snapshot first

```sh
snapshot-load --snapshot <chain>.bin --tables atomic --chain <chain> \
  --mongo mongodb://localhost:27017 --mongo-prefix aatest --mongo-drop
# writes db aatest_<chain>: atomicassets-* / atomicmarket-* collections
```

## Run the checks

```sh
cd benchmark/atomicassets/validate
npm install            # mongodb driver

# Footprint + decoded-doc samples
node inspect.js aatest_waxtest

# Decode parity vs the live API (immutable_data is block-independent → must match exactly)
node crosscheck.js aatest_waxtest 150                                  # WAX testnet (default API)
node crosscheck.js aatest_wax     150 https://wax.api.atomicassets.io  # WAX mainnet
```

## What "match" means

- **immutable_data** never changes after mint, so it must be an exact match regardless of the
  snapshot block height. (Key *order* is ignored — eosio-contract-api stores JSONB.)
- **mutable_data** may legitimately differ if it was edited after the snapshot block — reported
  separately, not a failure.
- A note on `data`: our asset `data` is the asset's own immutable+mutable merge (mutable wins). The
  API's `data` *also* folds in the template's attributes (a request-time join we keep normalized), so
  don't diff `data` directly — diff `immutable_data` / `mutable_data`.

### Validated: WAX testnet, block 409250749 (2026-06-04)

89.6M docs, 0 decode errors. **150/150 immutable_data + 60/60 mutable_data + 150/150 structural**
exact matches vs `test.wax.api.atomicassets.io`. On-disk footprint (WiredTiger-compressed
`storageSize`, ~9×): **~2.8 GB of state data for 88.8M assets** (≈33 B/asset; 25.7 GB *logical*);
fully indexed ≈ **8 GB**, dominated by the `data.$**` wildcard. (Report on-disk `storageSize`, not the
uncompressed `size`.) The one parity fix this surfaced: asset `float`/`double` attributes render as
strings on the live API (templates keep numbers) — handled in `map_asset`.

## HTTP load benchmark (WormDB vs the Postgres atomicassets-api)

`http-bench.mjs` measures **end-to-end served latency + throughput** for the read path. Cycle B made
WormDB serve the identical eosio-contract-api shape + query params as the reference Postgres
`atomicassets-api`, so the **same URL corpus hits both** and the comparison is apples-to-apples.

```sh
# one or both targets; same corpus, run sequentially so the client never self-contends
WORMDB=http://127.0.0.1:6390 ATOMIC=https://wax.api.atomicassets.io \
  N=50000 C=100 STATS_WORMDB=aa-wormdb OUT=wax-run node http-bench.mjs
```

It samples a real corpus (ids / owners / collections / (coll,schema) pairs, newest+oldest pages), runs a
weighted mix — `point` `/assets/:id`, `coll`, `owner`, `faceted` (coll+schema), `browse`, `account` —
and reports per-type + overall **p50/p95/p99** (min/mean/max + a latency histogram in the JSON) and
**req/s**. Writes `<OUT>.json` + `<OUT>.md`.

| env | meaning |
|---|---|
| `WORMDB` / `ATOMIC` | target base URLs (set one or both) |
| `N` / `DURATION` | requests per target, or seconds of steady-state load (`DURATION` wins) |
| `C` | concurrency |
| `SAMPLE` / `SAMPLE_FROM` | corpus size / base URL to sample from (default `WORMDB`) |
| `MIX` | override weights, e.g. `MIX=point=50,coll=20,owner=10,faceted=10,browse=5,account=5` |
| `STATS_WORMDB` / `STATS_ATOMIC` | container to sample CPU/RSS via `docker stats` during that run |
| `OUT` | results-file prefix (default `bench-results`) |

**Proving run = WAX 232M on native Linux**, both targets on the same data. Note that latency on the
**Windows Docker-Desktop loopback adds ~2–4 ms** and a tiny testnet segment makes postings trivial — so a
jungle4 run validates the harness but is *not* a proving number. The WSEG micro-bench already shows the
storage win (~33×) + µs in-process lookups; this harness is the served-HTTP p50/95/99 + throughput half.
