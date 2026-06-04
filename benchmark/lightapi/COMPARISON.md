# Operator-perspective comparison: our light-api vs cc32d9 eosio_light_api

Both serve the identical cc32d9 HTTP API and are fed the **same Libre snapshot**. This document
compares them from the perspective of someone who has to *deploy and run* them. Measured numbers go
in the **Results** section — run the harness, then fill them in.

> **See also [WORMDB_MATRIX.md](./WORMDB_MATRIX.md)** — a per-endpoint throughput/memory matrix adding
> a third serving engine (**WormDB**, Zig compiled-in stored procedures, single binary). Headline: the
> in-database engine is flat ~30K req/s across *all* endpoints while both database-backed stacks
> (this one and cc32d9) collapse ~7× on the 6-source `/account` + `/accinfo` assemblies. It also
> exposed a real bug here: **`/usercount` ran a live `permissions.distinct()` scan = ~30 s/request**.
> **Fixed** — `light-api` now serves `/usercount` + `/holdercount` from a non-blocking,
> background-refreshed cache (30 s → ~2 ms), and ensures the query-path indexes at startup. Critical
> for chain scale (WAX).

## 1. Architecture

**Our stack** — 3 moving parts, both consuming the snapshot directly:

```
Libre snapshot.bin ──► snapshot-load (Rust) ──► MongoDB ──► light-api (Rust/axum) ──► HTTP
```

**cc32d9 stack** — 5 moving parts, plus a full node:

```
Libre snapshot.bin ──► nodeos (Spring, SHiP) ──► Chronicle ──► lightapi_dbwrite.pl ──► MariaDB
                                                                                          │
                                                              lightapi.psgi (Starman) ◄───┘ ──► HTTP
```

The cc32d9 design is a **streaming indexer**: it never reads the snapshot itself; it relies on nodeos
to *replay* the snapshot's state out through SHiP as a delta stream that Chronicle consumes. Our
design reads the snapshot's binary state sections directly.

## 2. Components & dependencies

| | Our stack | cc32d9 stack |
|---|---|---|
| Processes to run | 2 (loader one-shot, server) | 5 (nodeos, Chronicle, dbwrite, Starman API, [wsapi]) |
| Datastore | MongoDB | MariaDB |
| Languages/runtimes | Rust (static binaries) | C++ (nodeos, Chronicle) + Perl + Node.js (wsapi) |
| Full node required? | **No** | **Yes** (Spring nodeos with SHiP) |
| Build/runtime deps | none beyond the binary + Mongo | mariadb-server, libmariadb, ~4 CPAN modules, Chronicle deb, nodeos deb, Node 22 |
| Config surface | one TOML | nodeos config.ini + Chronicle config.ini + per-network SQL setup + systemd units + env files |

## 3. Bootstrap (time-to-full-state)

Both start from the **same snapshot**, but get to a queryable full state very differently:

| | Our stack | cc32d9 stack |
|---|---|---|
| Path | `snapshot-load --tables lightapi` reads the binary state → bulk-insert to Mongo | nodeos loads snapshot → must advance a few blocks so SHiP makes the snapshot block irreversible → Chronicle scans the first-block **full-state delta** → dbwrite inserts row-by-row → MariaDB |
| External requirement | none | a working **Libre p2p peer** (to advance the node) |
| Dominant cost | one parallel binary pass | nodeos snapshot load + the giant first-block delta replayed through Chronicle → Perl → SQL |
| Reuses prior work | drops & reloads collections | Chronicle keeps an LMDB dedup DB; can `--save-snapshot` its state for reuse |

The cc32d9 first-block delta is the entire chain state, replayed one row at a time through a Perl
writer into SQL — inherently far slower than a direct binary load. (cc32d9's own WAX bootstrap notes
"approximately 9 hours" for that initial load; Libre is much smaller, but the *ratio* is the point.)

## 4. Resource footprint (qualitative)

| | Our stack | cc32d9 stack |
|---|---|---|
| RAM | MongoDB cache + a lightweight server | nodeos chainbase (GBs) + Chronicle LMDB + MariaDB buffer pool + node |
| Disk | Mongo data | nodeos state+blocks+state-history + Chronicle LMDB + MariaDB |
| CPU at idle | minimal | nodeos keeps following head (continuous) |
| Ongoing liveness | re-run the loader, or pair with a live Hyperion Mongo | nodeos+Chronicle keep MariaDB live in real time |

## 5. Operational considerations

- **Updates / liveness.** cc32d9 is inherently live (Chronicle tails the chain). Our loader is a
  point-in-time snapshot; for liveness it's designed to sit on the **same MongoDB a Hyperion
  deployment already keeps current** — i.e. "free" if you already run Hyperion.
- **Failure surface.** Our stack: Mongo + one server. cc32d9: a node that must stay synced, a
  Chronicle receiver, a websocket DB writer, a SQL server, and the API — more to monitor and restart.
- **Multi-chain.** cc32d9 puts many networks in one MariaDB (per-network tables) behind one API. Our
  server is multi-chain via `[[networks]]` over per-chain Mongo DBs. Both do multi-chain.
- **Maturity.** cc32d9 is production-proven across EOS/WAX/Telos/Libre/Proton for years, with a
  websocket bulk API and holder-count cron. Ours is new, though all 16 endpoints are now at parity.
- **Portability.** Our binaries are self-contained (pure-Rust deps, no C toolchain). The cc32d9 stack
  pulls platform-specific debs (nodeos, Chronicle) tied to Ubuntu versions.

## 6. Endpoint coverage

Both serve the 16 cc32d9 endpoints — including `/codehash`, now that `snapshot-load` emits
`account_codehash` from the snapshot's `account_metadata_object` section (`--tables lightapi`). The
cc32d9 stack also ships a **WebSocket bulk API** (`wsapi`); this MongoDB harness doesn't build it, but
the segment-based WormDB engine does — see [`WORMDB_MATRIX.md`](WORMDB_MATRIX.md).

## 7. Results

Environment: Docker Desktop on Windows (host overhead — a native Linux host would be faster).
Snapshot: `libre-2026-06-01_00-00`, head block **245,975,500**, 38.7 MB compressed / 147 MB `.bin`,
358,041 accounts. *(cc32d9 column pending a run of that stack.)*

### Time-to-full-state (same snapshot, both stacks)
| Stage | Our stack | cc32d9 stack |
|---|---|---|
| Download + extract snapshot | (shared, ~39 MB) | (shared) |
| Load to queryable full state | **25.2 s** (snapshot-load `--tables lightapi`, 0 errors) | **~13 min** (nodeos snapshot load + Chronicle scanning the snapshot block's full-state delta → single-threaded Perl writer into MariaDB, CPU-bound at 100% of one core) |
| — of which permissions + pub_keys | 19.5 s (1,422,911 docs) | (part of the ~13 min) |
| — of which accounts/voters/tables | 0.5 s (184,367 docs @ 395K docs/s) | |
| — of which index build | 0.8 s | |

**~30× faster bootstrap** for the same 1.5M-row dataset — our loader reads the snapshot's binary
state directly and bulk-inserts in parallel; cc32d9 must replay it block-by-block through nodeos→SHiP
→Chronicle→Perl→SQL.

### Parity — all 16 endpoints
Verified against the cc32d9 reference stack on the same Libre snapshot. **Full shape/format parity**
on every endpoint (`/networks /account /accinfo /balances /tokenbalance /topholders /topram /topstake
/holdercount /usercount /key /codehash /rexbalance /rexraw /sync /status`), including `?pretty=1`,
the `code:{code_hash}` field, `resources:null`, rex omission on rex-disabled chains, plain-text CRLF,
and the `Invalid count: N` error. Bugs the comparison surfaced and we fixed: `/balances` wrapper
object, accinfo `code`/`resources`/`rex` handling, `/codehash` object shape + chain block, `/status`
per-chain format, rex-disabled plain-text. Residual *data* differences are not bugs:
- cc32d9 follows chain head, so active accounts drift a few blocks from our snapshot instant.
- `/sync` + `/status` delay **number** differs — our snapshot DB has no `@block_time`; a live Hyperion
  Mongo supplies it. Format matches.
- `/usercount` + `/holdercount`: cc32d9 returns `0` until its 5-min holder-count cron runs; ours
  computes them live (358,041 accounts / 3,049 LIBRE holders) — arguably more correct.

### Throughput / latency (`/api/balances/libre/eosio.token`, n=3000, c=50, ApacheBench)
| | req/s | p50 | p95 | p99 | failed |
|---|---|---|---|---|---|
| Our light-api (Rust/axum + Mongo) | **6,503** | 7 ms | 12 ms | 13 ms | 0 |
| cc32d9 (Perl/Starman + MariaDB) | 4,074 | 12 ms | 14 ms | 15 ms | 0 |

~60% higher throughput and lower median latency for our stack; cc32d9 has a slightly tighter tail.
Both flawless (0 failures). Measured through Docker Desktop on Windows — a native Linux host lifts
both.

### Resource snapshot at idle (`docker stats`)
| Service | MEM | CPU |
|---|---|---|
| ours-light-api | | |
| mongodb | | |
| cc32d9-nodeos | | |
| cc32d9-lightapi (mariadb+chronicle+perl) | | |

## 8. Summary

- **Fast bootstrap, few parts, no node** is our stack's advantage — especially when a Hyperion
  MongoDB is already running, where serving the Light API becomes nearly free.
- **Battle-tested, inherently live, richer surface (wsapi)** is cc32d9's advantage — at the cost of a
  full node + Chronicle + SQL pipeline that is heavier to stand up and operate.
