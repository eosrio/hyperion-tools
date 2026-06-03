# DRAFT — Light API, served straight from your Hyperion MongoDB

> Draft community announcement. Numbers marked `__` are filled from a real benchmark run
> (`scripts/benchmark.sh` + `parity-check.sh` → `COMPARISON.md`). Tone: collaborative, credits cc32d9.

## TL;DR

If you run a **Hyperion** history node, you already hold the state that powers the
[cc32d9 `eosio_light_api`](https://github.com/cc32d9/eosio_light_api) — token balances, voters,
permissions, resources, REX. We built a small **Rust + axum** server (`light-api`) that serves the
**full cc32d9 Light API** directly from that MongoDB, plus a loader that bootstraps the same
collections from a portable chain snapshot in **seconds**. No second database, no extra node.

## The operator pain today

The reference Light API is a proven but heavy pipeline: **nodeos + SHiP → Chronicle → MariaDB →
Perl writer → Starman API** (five moving parts and a full node). Standing it up for a new chain means
running a node, a Chronicle receiver, a SQL server, and the Perl services — and the initial state load
replays the snapshot's entire first-block delta one row at a time through Perl into SQL (cc32d9's own
WAX bootstrap notes ~9 hours).

## What we built

- **`light-api`** — a tokio/axum server reproducing all 16 cc32d9 endpoints over MongoDB (multi-chain,
  `?pretty=1`, plain-text endpoints, exact error strings).
- **`snapshot-load --tables lightapi`** — reads an Antelope portable snapshot's binary state directly
  and bulk-loads `accounts`, `voters`, the eosio system tables, `permissions` and the `pub_keys`
  reverse index into MongoDB. No node, no Chronicle, no replay.

The big realization: **both approaches are snapshot-based** — a snapshot-loaded nodeos exposes full
state as the first SHiP block's delta, which is how cc32d9 bootstraps without a genesis replay. We
just read that same snapshot directly instead of streaming it through a node.

## The numbers (Libre, same ~39 MB snapshot for both)

| | Our stack | cc32d9 reference |
|---|---|---|
| Components | 3 (Mongo, loader, server) | 5 (nodeos, Chronicle, MariaDB, writer, API) |
| Full node required | No | Yes |
| Time to queryable full state | **25 s** | **~13 min** (~30× slower) |
| Throughput `/balances` (req/s) | **6,503** | 4,074 |
| p50 / p99 latency | 7 ms / 13 ms | 12 ms / 15 ms |
| Endpoint parity | 16/16 shapes match | (reference) |

*Measured on Docker Desktop / Windows; a Linux host lifts both. Reproduce with the harness in
`benchmark/`. Full detail + methodology in `COMPARISON.md`.*

Parity was verified by diffing every endpoint against the cc32d9 reference stack on the identical
snapshot — and that process found and fixed several shape bugs in our first cut (e.g. `/balances`
wrapper, `/codehash` object shape, rex-disabled handling). The remaining differences are not bugs:
cc32d9 follows chain head (a few blocks of drift) and computes holder counts on a 5-minute cron.

## Why this matters with Hyperion

If Hyperion already keeps this state current in MongoDB, serving the Light API becomes **nearly free**
— point `light-api` at the same database and you're done. For a cold chain, `snapshot-load` gets you
to full state in seconds instead of hours.

## Honest caveats

- cc32d9's stack is **battle-tested across many chains for years** and ships a WebSocket bulk API we
  haven't matched. This is a complement, not a replacement.
- `/codehash` is a known follow-up on our side (one snapshot section left to wire). The other 15
  endpoints are at parity.
- This is young software — **please try it and tell us where it diverges.**

## Try it / help us

Benchmark harness (both stacks, same Libre snapshot): `benchmark/` in `eosrio/hyperion-tools`.
Run `parity-check.sh` and open issues for any `DIFF` you find — that feedback directly hardens parity.

Thanks to **cc32d9** for the Light API spec and the reference implementation that made this possible.
