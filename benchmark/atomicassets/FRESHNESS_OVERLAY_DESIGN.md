# AtomicAssets WormDB store — live freshness overlay (serve from chain head, not a frozen snapshot)

The frozen `.wseg` store proves storage + query efficiency, but a snapshot is a *benchmark*, not a
product: to be production-ready it must serve the **chain head** — apply mints / transfers / burns /
data-updates from the Hyperion SHiP delta feed in real time while keeping reads sub-µs and correct.

This design was chosen by an adversarial design pass (4 architectures, each critiqued; full transcript
in the workflow log). All four naïve approaches hit the same three structural landmines; the synthesis
below is built from the highest-scoring **re-validation LSM spine** and dodges every fatal flaw the
critics found.

## The model: immutable base + in-RAM delta tip + re-validating reads

```
            ┌───────────────────────────── LiveSeg ──────────────────────────────┐
  reads ───▶│  Overlay (in-RAM, RwLock / ArcSwap)        Base segment (mmap, RO)  │
            │  ───────────────────────────────────       ───────────────────────  │
            │  fwd:  HashMap<asset_id, FwdState>     +    TABLE_AA_FWD (11)         │
            │  add:  per-(dim,key) RoaringTreemap         BY_OWNER/COLL/SCHEMA/TMPL │
            │  rem:  per-(dim,key) RoaringTreemap         DATA_ATTR (12..16)        │
            │  tomb: RoaringTreemap (burned ids)          SORTED_ID / SORTED_TMPL   │
            │  mint_seq: per-template u32 counter         (18 / 21)                 │
            │  wal + undo ring (post-LIB blocks)                                    │
            └─────────────────────────────────────────────────────────────────────┘
                                  ▲ single SHiP writer applies deltas per block
```

**The one invariant that makes it correct (the spine):** the *forward record* is the SOLE arbiter of an
asset's current owner / collection / schema / facet value. The base inverted postings only **propose**
candidate `asset_id`s; a candidate is yielded only after the live forward view (overlay `fwd` first, else
base `TABLE_AA_FWD`) confirms it still matches the key, and isn't tombstoned. So stale entries in the
immutable base postings are *harmless* — a transferred-out asset still physically sits in `by_owner[A]`,
but the read drops it because `fwd[X].owner == B`. Transfers and burns need **no base-posting surgery**.

The per-key `add`/`rem` bitmaps are the second half: `rem[key]` ANDed against the base posting keeps the
read's candidate head **dense** (so a whale that sold off its newest 256 doesn't truncate page 1), and
`add[key]` carries new members (mints, transfer-ins). `count(key) = base_len(key) + |add| − |rem|`.

## How each landmine the critics found is dodged

| Landmine (all designs hit ≥1) | Fix in this synthesis |
|---|---|
| **HEAD_K=256 cap**: id #257+ only via full `to_roaring()` (multi-ms); over-scan past stale candidates truncates pages | `rem[key]` keeps the base head dense → page-1 from `add ∪ (base_head \ rem \ tomb)`, bounded. Deep pages (rare) materialize `(base ∪ add) \ rem \ tomb` **once per scroll**, cursor-cached by `(key, epoch)` — the base store's pre-existing deep-page cost, **not amplified** per page. |
| **template_mint collides under burns** (re-seed from live `len()`) | Base stores **dense ranks 1..len** in the immutable `full_count` field, which never shrinks on tombstone. New mints get `base_len(tmpl) + monotonic_seq` → provably never collide with a base ordinal (1..base_len). `template_mint` here is a **sort key** (order-faithful); the displayed on-chain mint number is a history field (→ ES), unchanged. |
| **RAW asc vs ROARING desc head ordering** (hidden by order-insensitive `head_xor`) | The merge layer always collects the bounded candidate set into a buffer and **sorts desc explicitly** before paging — never trusts the raw head's intrinsic order. |
| **Counts drift under A→B→A churn** (non-idempotent `+/-1`) | `add`/`rem` are **sets** (roaring) → idempotent. Base membership ("was X in base `by_owner[A]`?") is one cheap **base-FWD lookup** (`base_fwd[X].owner == A`), not a roaring AND on the hot path. Round-trips cancel. |
| **Facet key un-rebuildable from base blob** (collection/schema stored name-encoded u64, lossy) | Facet keys recomputed from the **SHiP delta strings** (which carry collection/schema/field/value); the prior value `V_old` read from the base blob's stored attr (field_idx→value is recoverable). Each facet value is its own `data_attr_key`, so a non-facet edit never drops an asset from its facet posting. |
| **Compaction delta hand-off double-applies** (no per-block provenance) | WAL is block-indexed; compaction seals **behind LIB**, folds via the existing `AtomicBuilder`, and the post-compaction overlay replays only blocks `> sealed_block` from the WAL. No "subtract the folded portion" guesswork. |
| **Forks / RAM unbounded under reader lag** | **Seal behind LIB** → the immutable base/minis are fork-proof; reorgs only touch the in-RAM tip (`rollback_to(block)` replays the WAL window). RwLock (bounded RAM) for the POC; ArcSwap-per-block is the lock-free upgrade. |

## Apply path (single SHiP writer, per block)

- **MINT** X (new asset_id, largest so far): `fwd[X]=Live{..}`; `add[owner/coll/schema/tmpl/facet] += X`;
  `tomb` untouched; `template_mint = base_len(tmpl)+mint_seq[tmpl]++`; X concatenates at the **front** of
  the desc browse (monotonic id) and into `sorted_tmpl_adds`.
- **TRANSFER** X (A→B): read base owner `O = base_fwd[X].owner` (one lookup); update `fwd[X].owner=B`;
  for A: if `O==A` (base member) `rem[by_owner A]+=X` else `add[by_owner A]-=X`; symmetric un-remove/add
  for B. No other index touched.
- **BURN** X: `fwd[X]=Tombstone`; `tomb += X`. All postings/orderings filter it out via the spine.
- **SETDATA** X (facet field F: V_old→V_new): `fwd[X].mutable` updated; `rem[data_attr(F,V_old)] += X`,
  `add[data_attr(F,V_new)] += X`. Non-facet fields: forward-only update.
- **schema / template / collection** create/modify: small forward overlays (rare).

## Read merge (per query)

- **Q1 point**: `fwd` first (Live → return / Tombstone → 404), else base FWD. O(1).
- **Q2/Q4 owner/collection page-1**: candidates = `add[key]` (largest) ∪ `base_head(256) \ rem \ tomb`;
  sort desc; **re-validate** each via forward (drop if `fwd[X]` moved it off `key` or tombstoned); hydrate
  100. Validation piggybacks on the hydration the page already does.
- **Q3 facet page-1**: same, keyed by `data_attr_key`.
- **Q5 browse**: `sorted_id_adds` (desc, all new mints) **concatenated before** the base SORTED_ID blob —
  O(1) start, correct because mint asset_ids are strictly monotonic; tombstones skipped on slice.
- **Q6 sort-by-mint**: merge `sorted_tmpl_adds` into the base SORTED_TMPL slice; hydrate.
- **multi-attr AND**: `to_roaring()` the two base postings, `∪ add \ rem \ tomb` each, AND.

## What the POC measures (the actual answer to "can we serve it live")

1. **Apply throughput** — mutations/sec, single writer (must clear SHiP burst rates, ~10⁴/s).
2. **Merged read latency** — Q1–Q6 page-1 after applying 1M / 10M deltas: p50 must stay sub-µs / low-µs.
3. **Correctness** — transfer X(A→B) ⟹ X in `by_owner[B]` page, absent from `by_owner[A]`, counts move;
   burn ⟹ gone everywhere + count drops; mint ⟹ first in browse + point-lookupable; setdata ⟹ moves
   between facets; **fork rollback** ⟹ a transfer reverts.
4. **Concurrent freshness** — a writer applies the stream at SHiP rate while a reader serves the workload:
   reader throughput/latency + the lag from "block applied" to "visible in a query".
5. **Overlay RAM** vs mutation count (the compaction trigger).

Implementation: `crates/wseg-build/src/aa_live.rs` (Overlay + LiveSeg) + bin `aa-live` (apply stream +
bench + correctness). Compaction reuses `AtomicBuilder` + atomic mmap remap.

## Measured (BUILT — WAX mainnet 232.3M base segment, 2026-06-04)

Driven by a realistic SHiP-shaped stream (35% mint / 45% transfer / 12% burn / 8% setdata) sampled from
real owners/collections/facets in the segment (`aa-live --seg aa-mainnet-hybrid.wseg`). Single box.

| | result |
|---|---|
| **apply throughput** (single writer) | **370k mutations/s** — 37× a 10⁴/s SHiP burst |
| **freshness lag** (commit write-lock hold) | **p50 276 µs / p99 498 µs** (pure in-RAM; base reads happen lock-free in `prepare`) |
| **merged reads with overlay active** | Q1 point **0.3 µs** · Q3 facet **0.3 µs** · Q4 collection **0.6 µs** · Q5 browse **3.2 µs** · Q2 owner-page **4.2 µs** · Q6 sort-by-mint **8 µs** — all sub-µs/low-µs, unchanged from frozen |
| **overlay heap** | **114 B / mutation** (218 MB for 2M; 435 MB for 4M) — the base mmap page cache RSS is separate + evictable |
| **concurrent** (1 writer + 6 readers) | writer **278k mut/s** WHILE readers serve **557k req/s**, reader p50 **1.1 µs** |
| **correctness at scale** | transfer→new owner PASS · burn→gone PASS · **page-merge invariant: 0 stale across 38,974 page entries / 5,000 owners** |
| **fork rollback** | a transfer reverts on a reorg (WAL replay) — unit-tested |

The two-phase apply (lock-free `prepare` resolves all base mmap reads, brief `commit` mutates in RAM) is
what keeps the write-lock hold — and thus freshness lag — at hundreds of µs instead of stalling readers
on a cold page fault. The validate-in-RAM read merge (a base-head candidate absent from the overlay is, by
definition, still a member of its key) keeps page reads sub-µs: no base decode in the read path.

**Honest edges:** an occasional ~9 ms max commit (allocator/roaring-grow jitter; p99 is 498 µs) — smaller
blocks bound it. Deep (page-N) pagination on a hot key materializes the base posting once per scroll
(the frozen store's pre-existing cost, not amplified) — cursor pagination is the production fix. The
displayed on-chain `template_mint` number is a history field (→ ES); the overlay's ordinal is the
order-faithful sort key. Compaction (fold overlay → fresh segment behind LIB, atomic mmap remap) is
designed + reuses `AtomicBuilder`, not yet measured end-to-end.
