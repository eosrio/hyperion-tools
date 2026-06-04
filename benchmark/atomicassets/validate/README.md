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
exact matches vs `test.wax.api.atomicassets.io`. State footprint (incl. all indexes): **31.3 GB**
for 88.8M assets (≈352 B/asset). The one parity fix this surfaced: asset `float`/`double` attributes
render as strings on the live API (templates keep numbers) — handled in `map_asset`.
