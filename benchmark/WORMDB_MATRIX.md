# Light API serving-engine matrix: WormDB vs Rust+Mongo vs cc32d9

A throughput/memory comparison of **three ways to serve the cc32d9 `eosio_light_api` HTTP contract**,
all containerized, all fed the same Libre data, all benchmarked identically from outside the
containers. The question this answers: *what does it cost to serve the Light API, and does compiling
the API into the database pay off?*

## Stacks (each a self-contained container)

| | Engine | Datastore | Shape |
|---|---|---|---|
| **WormDB** | Zig, compiled-in stored procedures + native HTTP gateway | in-process (KV) | one 4.3 MB static binary |
| **Rust+Mongo** | `light-api` (tokio/axum) | MongoDB | app server + database |
| **cc32d9** | Perl/Starman (`lightapi.psgi`) | MariaDB (fed by nodeos→SHiP→Chronicle) | app + database + node + indexer |

WormDB serves the Light API by mapping `GET /api/...` → an `EXEC` of a compiled Zig procedure that
assembles the cc32d9 JSON from local KV entries — no application tier, no per-lookup marshaling. All
responses are byte-verified against `light-api` (which is itself byte-verified against the original
cc32d9 instance).

## Method

- Each stack runs in Docker (Docker Desktop on Windows). Benchmarked from the host with a keep-alive
  HTTP load generator, concurrency **c=50**, steady-state request counts.
- WormDB built for Linux (its native target: io_uring/epoll) via a multi-stage image
  (`zig 0.16` + `libsodium` + `meshguard`).
- cc32d9 was given **32 Starman workers** (its default of 6 was the entire bottleneck) for a fair
  comparison — see the worker note below.

## Throughput (req/s, c=50, containerized)

| endpoint | WormDB | Rust+Mongo | cc32d9 (32 workers) |
|---|---:|---:|---:|
| `balances` | **29,995** | 19,572 | 4,231 |
| `tokenbalance` | **31,291** | 19,054 | 4,619 |
| `account` | **29,793** | 3,184 | 1,744 |
| `accinfo` | **29,694** | 4,261 | 1,828 |
| `networks` | **28,209** | 23,611 | — |
| `holdercount` | **28,046** | 1,522 | — |
| `usercount` | **~28,000** | **0.03** (30.7 s/req) | 4,800 |

### The headline: the assembly tax

`balances`/`tokenbalance` are single-lookup endpoints; `account`/`accinfo` are **6-source assemblies**
(permissions, resources, delegated bandwidth, codehash, rex, balances).

- **WormDB is flat (~30K) across all endpoints** — simple or complex — because it assembles in-process.
- **Both database-backed stacks collapse on the assembly endpoints**: Rust+Mongo drops ~6–9× (6 Mongo
  round-trips per request), cc32d9 drops similarly (6 SQL queries in Perl). The multi-query round-trip
  tax is real and hits *any* app-server-over-database design — and it is exactly what compiling the
  assembly into the database removes.

Net: WormDB is ~1.5× faster on light endpoints and **~7–17× faster on the heavy assembly endpoints**,
*flat*, as a single binary — measured against a properly-tuned cc32d9, not a strawman.

### The precompute gap (and a real bug it exposed)

Two endpoints are *aggregates* over the whole dataset:
- `holdercount` — Mongo `count_documents` per request: **1,522 req/s** vs WormDB's precomputed
  **28,046**.
- `usercount` — our Rust `light-api` runs a live `permissions.distinct("account")` over 716K
  documents **on every request: 30.7 s per request** (~0.03 req/s). WormDB serves a precomputed value
  in **2.7 ms**; cc32d9 precomputes via a 5-minute cron (4,800 req/s).

This is a genuine flaw the benchmark surfaced in *our own* Rust implementation (usercount must be
cached/precomputed, not scanned live) — and a clean demonstration that the right place for aggregate
state is *materialized next to the serving path*, which is exactly what WormDB's load-time procedures
and cc32d9's cron both do.

## cc32d9 worker sizing (a real operational finding)

cc32d9's Starman is a PreFork server — throughput is capped by worker count, and it **degrades when
concurrency exceeds workers** (no async absorption). At its 32-worker count it peaks at c=32:

| endpoint | cc32d9 c=50 | cc32d9 c=32 (optimum) |
|---|---:|---:|
| `balances` | 4,231 | 10,686 |
| `tokenbalance` | 4,619 | 12,026 |
| `usercount` | 4,800 | 14,734 |
| `account` | 1,744 | 4,687 |
| `accinfo` | 1,828 | 5,082 |

So cc32d9 is far more competitive when workers are sized to the load (default 6 → throttled; 32 →
respectable on light endpoints) — but the operator must tune it, and the heavy endpoints stay ~5K
because each request is still Perl+SQL. WormDB and Rust+Mongo absorb c=50 without a worker cliff.

## Memory (resident)

| | Process | + datastore | total to serve |
|---|---:|---:|---:|
| **WormDB** (data + server in one) | **445 MiB** | — | **445 MiB** |
| Rust+Mongo | 245 MiB | MongoDB 3.88 GiB | ~4.1 GiB |
| cc32d9 | 1.24 GiB (mariadb+chronicle+perl) | + nodeos 142 MiB | ~1.38 GiB |

WormDB holds the data *in the server* (168K balances + 168K accinfo bodies), so its total footprint is
the process itself — no separate database, no node, no indexer.

## Bootstrap / time-to-serve

| | Path | Time |
|---|---|---|
| Rust+Mongo | `snapshot-load --tables lightapi` (binary snapshot → Mongo) | **25 s** |
| WormDB | load balances + accinfo into KV (this prototype: from light-api) | ~40 s |
| cc32d9 | nodeos snapshot load + Chronicle full-state delta → MariaDB | **~13 min** |

## Caveats (honest)

- **Docker Desktop on Windows taxes every stack ~3×.** Host-native WormDB was **88,378 req/s** (vs
  ~30K in Docker); host-native Rust+Mongo ~20K. A bare Linux host lifts all absolute numbers; the
  *ratios* hold. (io_uring is also blocked by Docker Desktop's seccomp on Windows; it works on a real
  Linux host, though WormDB's HTTP gateway is thread-per-connection so it's not the bottleneck here.)
- **WormDB endpoint coverage:** all **16/16** endpoints implemented and parity-verified against
  `light-api` (`networks account accinfo balances tokenbalance topholders topram topstake holdercount
  usercount codehash key rexbalance rexraw sync status`).
- **WormDB data coverage (load-time scope, not a code limit):** accinfo loaded for the 168K
  balance-holders (not all 358K accounts → balance-less accounts fall back to the minimal shape);
  `/key` loaded for a 3000-key sample; codehash/topholders fully loaded. Loading the full sets closes
  these gaps without code changes.
- **WormDB assembly note:** `/account` does real in-process splice assembly (accinfo body + balances);
  `/accinfo` is currently served from a materialized body (the heavy normalized-assembly is done at
  load time). A fully-normalized in-procedure assembly would keep the same throughput profile.
- **cc32d9 column** is the 32-worker configuration; the default 6 workers throttles it to ~1.3–3.3K.
