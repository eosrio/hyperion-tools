# Light API parity & operator benchmark

Side-by-side comparison of **two ways to serve the cc32d9 `eosio_light_api`**, both fed the **same
Libre portable snapshot**:

| | Our stack | cc32d9 reference stack |
|---|---|---|
| Serve | `light-api` (Rust/axum) | `lightapi.psgi` (Perl/Starman) |
| Store | MongoDB | MariaDB |
| Bootstrap | `snapshot-load` reads the snapshot → Mongo (one binary) | nodeos loads the snapshot + SHiP → **Chronicle** reads the full-state delta → MariaDB |
| Components | 3 (mongo, loader, server) | 5 (nodeos, Chronicle, MariaDB, Perl writer, Perl API) |

Goal: **(1) verify response parity** between the two APIs, and **(2) compare them from an operator's
point of view** (setup, time-to-data, components, resources). See [COMPARISON.md](./COMPARISON.md).

## Why both can use a snapshot

A common misconception is that the cc32d9 stack needs a full genesis replay. It does not: when nodeos
loads a snapshot and enables SHiP, **the first block in `chain_state_history` carries the entire
chain state as deltas**, so Chronicle started at the snapshot block reconstructs full state. That
first-block delta is large, so Chronicle's initial load takes a while (minutes–hours depending on the
chain) — versus seconds for `snapshot-load`. That gap is the headline operator finding.

## Prerequisites

- Docker + Docker Compose v2. Disk/RAM are modest for Libre (a *small, low-activity* chain): the
  snapshot is **state, not history**, so it's light (tens–low-hundreds of MB). Budget a few GB of disk
  for nodeos state + MariaDB + Mongo, and ≥4 GB RAM. Heavier chains would need much more.
- `curl` and `jq` on the host for the parity script; optionally `oha`/`hey`/`ab` for the benchmark.
- **Libre p2p peers — for the cc32d9 stack only.** nodeos does *not* expose the snapshot block on
  SHiP at load time; the state-history log only gets that block's full-state delta **after nodeos
  applies at least one block past it**. So with zero peers, nodeos sits idle at the snapshot block,
  SHiP stays empty, and Chronicle ingests nothing → cc32d9's DB stays empty. The cc32d9 nodeos image
  **bundles a set of Libre seed peers** (probed reachable), so it works out of the box; add more via
  `LIBRE_P2P_PEERS` in `.env` if they go stale. **Our stack needs no peers at all** — it reads the
  snapshot file directly.

  > On a quiet chain like Libre, once nodeos is nudged past the snapshot there is almost no state
  > change, so the cc32d9 node "following head" drifts negligibly from the snapshot instant our DB
  > holds — parity stays effectively snapshot-pinned.

## Layout

```
benchmark/lightapi/
  docker-compose.yml      # the whole stack (profiles: ours | cc32d9 | all)
  .env.example            # snapshot URL, Libre peers, ports
  snapshot/               # one-shot: download + extract snapshot, derive block_num
  ours/                   # our Dockerfile, light-api.toml, loader/server entrypoints
  cc32d9/nodeos/          # Spring 1.2.2 nodeos (snapshot + SHiP)
  cc32d9/lightapi/        # MariaDB + Chronicle 3.3 + Perl writer + Starman API
  scripts/                # parity-check.sh, benchmark.sh, accounts.txt
```

## Run

```bash
cd benchmark/lightapi
cp .env.example .env
# edit .env: set LIBRE_P2P_PEERS=<host:port> (needed for the cc32d9 side)

# Our stack only (fast — snapshot → Mongo → server, minutes):
docker compose --profile ours up --build

# The reference stack only (nodeos sync + Chronicle full-state load, slower):
docker compose --profile cc32d9 up --build

# Everything (both stacks side by side):
docker compose --profile all up --build
```

> The services are gated behind compose **profiles** (`ours`, `cc32d9`, `all`). A bare
> `docker compose up` only runs the shared `snapshot-fetch` step — always pass a `--profile`.

- Our API:    http://localhost:7000/api/...
- cc32d9 API: http://localhost:5001/api/...

The `snapshot-fetch` service downloads the Libre snapshot once into a shared volume; both stacks
consume the same `.bin`. `ours-loader` exits when the load completes; `ours-light-api` then starts.
The cc32d9 side is ready once Chronicle finishes its initial full-state load — watch:

```bash
docker compose logs -f cc32d9-lightapi    # look for the DB writer acking past the first block
docker compose logs -f cc32d9-nodeos      # should show it syncing past the snapshot block
```

## Check parity

```bash
./scripts/parity-check.sh                 # PASS/DIFF/ERR per account per endpoint
VERBOSE=1 ./scripts/parity-check.sh        # show the diff on mismatch
```

It strips the volatile `chain{}` block and key-sorts balances before diffing, so a PASS means the
substantive data matches. Expect small diffs only on accounts that changed in the few blocks nodeos
advanced past the snapshot (the cc32d9 node keeps following head; our DB is the snapshot instant).

## Benchmark

```bash
./scripts/benchmark.sh                     # throughput/latency for the same endpoint on both
N=5000 C=100 ./scripts/benchmark.sh
PATH_Q=/api/account/libre/eosio ./scripts/benchmark.sh
```

Record results in [COMPARISON.md](./COMPARISON.md).

## Notes & caveats

- **Libre peers are operator-specific** and change; a wrong/absent peer means the cc32d9 node never
  advances and Chronicle stalls. This does not affect our stack.
- The cc32d9 node **follows head** after bootstrap (a live node), so its state drifts slightly ahead
  of the snapshot instant our DB holds. Parity is exact for unchanged accounts.
- All 16 endpoints serve on both sides, including `/codehash` (`snapshot-load` now emits
  `account_codehash` from the snapshot's `account_metadata_object` section).
- This harness is for evaluation, not production. The cc32d9 stack runs MariaDB + Chronicle + nodeos
  in containers with modest tuning; numbers are indicative, not a tuned-vs-tuned shootout.
