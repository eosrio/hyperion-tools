# WAX-scale Light-API serving: WormDB (mmap segment) vs Rust+Mongo

> ## Update — binary segment + WebSocket API (2026-06-03 re-validation)
>
> Everything below was the first WAX run (a 13–14 GB segment of **pre-rendered JSON**, HTTP only).
> Since then the segment moved to a **compact binary encoding**, gained two reverse-index tables
> (`token_holders`, `pub_keys`), and the **cc32d9 WebSocket JSON-RPC API**. Re-measured at WAX scale:
>
> | | old (JSON segment) | **new (binary segment)** |
> |---|--:|--:|
> | segment file | ~14 GB | **7.04 GB** (balances 0.78 + accinfo 3.55 + token_holders 0.57 + pub_keys 0.66 GB blob) |
> | boot (mmap) | < 1 s | **54 ms** |
> | idle RSS | ~20 MiB | **20 MiB** |
> | RSS under HTTP battery | ~25 MiB | **23 MiB** |
> | endpoints working **standalone** (no feed/loader) | balances, accinfo, account, networks | **+ topholders, holdercount, usercount** (the two new tables) |
>
> **HTTP throughput holds** (c=50, binary segment, Docker): usercount 27.1K · balances 26.9K ·
> accinfo 24.7K · account 23.7K · topholders 23.9K · holdercount 27.0K — flat ~24–27K, p99 ~4 ms.
> All byte-parity vs light-api. (`holdercount` was 177 rps until an O(1) count was added to the
> `token_holders` blob header — it counted 12.48M lines per request; now a single read.)
>
> **WebSocket `get_token_holders` at scale** — streamed **all 12,481,173 holders** of
> `eosio.token/WAX` over the WS API: **883,261 rows/s**, 1.75 GB of JSON, 14.1 s end-to-end, first row
> at 190 ms, **RSS held at 23 MiB** throughout (mmap — no memory blowup streaming the chain's largest
> token). All 4 WS methods (`get_networks`, `get_balances`, `get_token_holders`,
> `get_accounts_from_keys`) parity-verified.
>
> **Headline:** the binary segment is **half the size** of the JSON one *and* serves more endpoints
> standalone, at the same flat ~25K-rps / tens-of-MiB-resident profile — now with the full HTTP **and**
> WebSocket cc32d9 surface.

---

The [Libre matrix](WORMDB_MATRIX.md) showed WormDB winning by compiling the API into the database —
but at 358K accounts everything fits in RAM. **WAX is ~60× bigger** (21.75M account universe, 17.57M
balance-holders, 43.6M permissions), and it breaks the original WormDB prototype, which stored
**pre-rendered JSON in a generic in-RAM hashmap**:

| WAX in the old (hashmap, pre-rendered) model | projected RAM |
|---|--:|
| `acci:` — 21.75M accounts × ~1 KB pre-rendered accinfo | ~24.8 GB |
| `bal:` — 17.57M holders × packed balances + overhead | ~3.5 GB |
| **total resident** | **~28–30 GB** |

…plus a multi-hour load (21.75M accinfo bodies fetched from the app + ~40M wire `SET`s). The
"445 MiB single binary" story does not survive the jump.

## The fix: a read-only, memory-mapped *frozen segment*

WormDB gained a new storage tier — an externally-built, `mmap`'d columnar file (`.wseg`):

- keys are the Antelope `name` **u64** in one contiguous **sorted index** (20 B/entry, binary search,
  **zero per-entry allocation**) over **blob arenas**;
- `mmap` → resident memory is the **working set the OS pages in**, not the whole dataset; boot is
  just `mmap` (the 13 GB file is never read at startup);
- the procedures assemble the cc32d9 JSON from the mapped blobs in-process — same flat-throughput
  serving path as before, now backed by a file instead of the heap.

Built once, offline, by `hyperion-tools/crates/wseg-build` (streams MongoDB → `.wseg`), byte-verified
against `light-api` (itself byte-verified against cc32d9).

## Throughput (req/s, c=50, WAX)

WormDB-wax is **containerized (Docker Desktop, ~3× tax)**; light-api is **host-native** (no tax) over
the same MongoDB. So WormDB's absolute numbers are *understated* ~3× relative to light-api here.

| endpoint class | endpoint | WormDB-wax (segment, Docker) | light-api (Mongo, host-native) |
|---|---|--:|--:|
| cached scalar | usercount | 27,236 | **54,604** |
| cached scalar | holdercount | 27,503 | **54,733** |
| single lookup | balances | **26,786** | 19,757 |
| single lookup | tokenbalance | **27,306** | 18,996 |
| **6-source assembly** | **accinfo** | **25,318** | 5,374 |
| **6-source assembly** | **account** | **25,039** | 4,508 |
| aggregate (top-N) | topholders | **23,121** | 6,654 |
| aggregate (top-N) | topram | **24,706** | 8,463 |
| aggregate (top-N) | topstake | **23,241** | 7,995 |

p99 latency: WormDB **flat ~4 ms** across all nine; light-api 1.3 ms (cached scalar) → 4.8 ms (lookup)
→ **15.6–17.7 ms** (assembly).

### The shape of it

- **WormDB is flat (~23–27K) across every endpoint** — simple, assembly, or aggregate — because it
  assembles in-process from the mmap. Even Docker-taxed, it beats host-native light-api on 7 of 9.
- **light-api swings 12×**: 54K on a cached scalar down to **4.5K on the 6-query `account` join**.
  The multi-round-trip assembly tax is the wall, and compiling the assembly into the DB removes it —
  **4.7× on accinfo, 5.6× on account, ~3× on the top-N aggregates**, and that's *before* removing
  WormDB's Docker handicap.
- The one place light-api wins is the **cached scalar** (usercount/holdercount): there's no assembly
  to save, so host-native axum serving one cached value beats Docker-taxed WormDB. Honest, and
  expected.

## Memory (resident) — the headline

| | serving the full WAX set |
|---|--:|
| **WormDB-wax** — idle (post-mmap, pre-query) | **20.5 MiB** |
| **WormDB-wax** — after single-hot-account battery | **25 MiB** |
| **WormDB-wax** — after a **30K-distinct-account** accinfo + account sweep (591K reqs) | **75 MiB** |
| WormDB-wax — hard ceiling (every one of 21.75M accounts touched) | ≤ 13 GB (the file) |
| light-api process + MongoDB | 245 MiB + ~4 GB |
| old WormDB hashmap model (projected) | ~28–30 GB |

A 13 GB dataset served at **tens of MiB resident** — because mmap pages in only the working set, and
binary search over a sorted index touches remarkably few pages even across 30K distinct accounts.
The old in-RAM model would need ~28–30 GB *always*; this needs ~75 MiB under a realistic broad load,
bounded by the 13 GB file only if literally every account is hit.

## Load / time-to-serve

| | path | time |
|---|---|--:|
| WormDB segment build | `wseg-build` streams Mongo → 13 GB `.wseg` (offline, one-time) | 588 s |
| WormDB **boot** | `mmap` the file (lazy — not read at startup) | **< 1 s** |
| light-api | `snapshot-load --tables lightapi` → Mongo (lean) then serve | ~15.5 min |

## Caveats (honest)

- **Docker tax**: WormDB-wax is containerized (~3× penalty on Docker Desktop / Windows; host-native
  WormDB hit 88K at Libre). light-api here is host-native. So the WormDB advantage on the heavy
  endpoints (4.7–5.6×) is *understated*; a like-for-like (both containerized, or both host-native)
  would widen it. The cached-scalar result would also flip back toward WormDB host-native.
- **RSS is access-pattern-dependent.** 25 MiB (hot key) and 75 MiB (30K-account sweep) are real
  measurements; a uniform sweep of all 21.75M accounts faults toward the 13 GB ceiling. The honest
  production range is tens-to-hundreds of MiB, not the whole file.
- **cc32d9 was not run at WAX** (its nodeos+Chronicle bootstrap is ~9 h). At Libre/32-workers it
  served accinfo ~1.8K / account ~1.7K — directional context only; the WAX comparison above is
  two-way.
- **Segment size**: 13 GB is pre-rendered-JSON-in-mmap (disk traded for builder simplicity; the RAM
  win holds regardless of file size). A binary-encoded perms variant (raw 33-byte keys + in-proc
  base58check) would cut it to ~3.5 GB.
- **Coverage**: 9 endpoints benchmarked + byte-parity verified (balances, tokenbalance, accinfo,
  account, topholders, topram, topstake, usercount, holdercount); networks/sync/status served from KV
  meta; codehash/key/rex loadable but not part of this WAX run.
- Balance/delegation **array order** may differ from light-api's DB order (parity normalized by sort,
  per the existing methodology).
