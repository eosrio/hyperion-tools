# light-api

A high-performance **tokio + axum** server that reproduces the
[cc32d9 `eosio_light_api`](https://github.com/cc32d9/eosio_light_api#http-api) HTTP API by reading
the per-chain MongoDB that `snapshot-load` writes and that a **Hyperion** deployment maintains live.

The goal: serve every Light API request from the same MongoDB an operator already runs for Hyperion —
no second database, no separate service stack.

## How it fits together

```
 snapshot (.bin/.tar.gz) ──snapshot-load --tables lightapi --mongo──▶  MongoDB  ◀── Hyperion live indexer
                                                                          │
                                                          light-api (this crate, reads only)
                                                                          │
                                                                   cc32d9 HTTP API
```

One MongoDB database per chain, named `<prefix>_<chain>` (e.g. `hyperion_eos`). Collections used:
`accounts`, `permissions`, `pub_keys`, `voters`, and the dynamic system tables `eosio-userres`,
`eosio-delband`, `eosio-rexbal`, `eosio-rexfund`, `eosio-rexpool`, plus `account_codehash`.

## Endpoints

All under `/api`, all GET, all honoring `?pretty=1`:

| Endpoint | Output |
|---|---|
| `/networks` | JSON array of per-chain `{network, sync, decimals, systoken, chainid, production, block_num, block_time, description}` |
| `/account/CHAIN/ACCT` | JSON `{account_name, chain, balances, resources, permissions, delegated_to, delegated_from, linkauth, rex}` |
| `/accinfo/CHAIN/ACCT` | as `/account` minus `balances` |
| `/balances/CHAIN/ACCT` | JSON `[{contract, currency, decimals, amount}]` |
| `/rexbalance/CHAIN/ACCT` | JSON `{account_name, chain, rex:{fund, matured, maturing, savings}}` |
| `/rexraw/CHAIN/ACCT` | JSON raw rexbal/rexfund/rexpool rows |
| `/key/PUBKEY` | JSON accounts using the key across all networks (`EOS…` or `PUB_K1_…`) |
| `/codehash/SHA256` | JSON accounts with the contract code hash across all networks |
| `/tokenbalance/CHAIN/ACCT/CONTRACT/SYM` | **text** numeric balance (`0` if none) |
| `/topholders/CHAIN/CONTRACT/SYM/N` | JSON `[["acct","amount"],…]`, N∈[10,1000] |
| `/holdercount/CHAIN/CONTRACT/SYM` | **text** count |
| `/usercount/CHAIN` | **text** account count |
| `/topram/CHAIN/N` | JSON `[["acct",bytes],…]` |
| `/topstake/CHAIN/N` | JSON `[["acct",cpu,net],…]` |
| `/sync/CHAIN` | **text** `<delay> OK\|OUT_OF_SYNC` |
| `/status` | **text** `OK\|OUT_OF_SYNC` |

## Configuration

A single TOML file (see [`light-api.toml`](./light-api.toml)) declares the Mongo connection, the HTTP
bind, and one `[[networks]]` block per chain (static `chain{}` metadata; `block_num`/`block_time`/
`sync` are read live from Mongo).

```bash
cargo run -p light-api --release -- --config light-api.toml
# overrides: --bind 127.0.0.1 --port 7000 --mongo-uri mongodb://host:27017
```

## Bootstrapping the data from a snapshot

```bash
# Load all Light-API collections for one chain into Mongo from a portable snapshot:
snapshot-load --snapshot snapshot-<id>.bin --chain eos \
  --tables lightapi --mongo mongodb://localhost:27017 --mongo-drop
```

`--tables lightapi` expands to `voters, accounts, eosio:{global,userres,delband,rexbal,rexfund,
rexpool}` plus the native `permissions` pass (which also builds the `pub_keys` reverse index). On a
live Hyperion deployment the indexer keeps these collections current; the snapshot load is only the
one-time backfill.

## Notes & limitations

- **`/sync` & `/status`** are meaningful against a *live-updated* Hyperion Mongo (the indexer fills
  `@block_time`). A static snapshot-only DB has no head-block time, so it reports `OUT_OF_SYNC`.
- **`/codehash`** requires the `account_codehash` collection. Emitting it from the snapshot's
  `account_metadata_object` section is the remaining loader task (needs validation against a real
  snapshot); until then `/codehash` returns `{}`.
- **REX `savings`** detection uses the eosio.system far-future maturity sentinel; verify against a
  live cc32d9 instance for chains that fork the rex contract.
- Read-only: the server never writes to Mongo. Place it behind your existing Hyperion reverse proxy.
