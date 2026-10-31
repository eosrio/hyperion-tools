# Hyperion 4.5 progress — 31 October 2026

MongoDB is the 4.5 state backend; Elasticsearch continues storing and serving history.
WormDB implementation and qualification are follow-up work. The accepted architecture still
requires full AtomicAssets/AtomicMarket 2.7.0 parity, Wire support, reversible-head recovery,
ABI-versioned declarative mapping reload and retained-history upgrades without a full reindex.

## Delivered components

[Tools #78](https://github.com/eosrio/hyperion-tools/pull/78) publishes the backend decision.
[SDK #41](https://github.com/eosrio/hyperion-sdk/pull/41) and
[HAPI #400](https://github.com/eosrio/hyperion-history-api/pull/400) carry original-action
authorization through the source/worker/controller interfaces.
[HAPI #399](https://github.com/eosrio/hyperion-history-api/pull/399) fixes the reproduced
worker binding acknowledgment race. [Atomic #49](https://github.com/eosrio/hyperion-atomicassets-plugin/pull/49)
extends the earlier buyoffer-ID component with bounded ordered asset bundles.
These merged changes passed their component checks; they do not establish release qualification.

## Active work

| Lane | Verified evidence | Next acceptance |
|---|---|---|
| [HAPI #401](https://github.com/eosrio/hyperion-history-api/pull/401): managed Wire and historical source authority | Local retained 1..1480 controls pass, but Linux CI at head 54b4 still exceeds the original 180-second limit. The PR remains unmerged. Actual full-workload measurement found 17.78 million field-validation calls occupying about 11 seconds of an instrumented local run. | Review the narrow allocation correction and separately named source/emission profiles, then pass the unchanged workload and exact Linux CI before merging. Microbenchmarks do not prove integrated savings. |
| [Tools #79](https://github.com/eosrio/hyperion-tools/issues/79) / [#80](https://github.com/eosrio/hyperion-tools/issues/80): server replay admission | The r79 component revalidation passed all 250 controls. Preparation failed before relay/services on a receiving import; its separate software postguard also failed. Two fresh read-only observations find empty private resources and unchanged protected processes/components, with two exact withdrawn Tailscale peer routes. The original full successor guard refuses that route delta. | Finish the narrowly versioned successor policy and receiving correction, independently review current full guards, then run the original replay/recovery/custody sequence. No baseline reset, old case adoption or relaxed limits. |
| [SDK #42](https://github.com/eosrio/hyperion-sdk/issues/42) → [HAPI #402](https://github.com/eosrio/hyperion-history-api/issues/402) → [Atomic #50](https://github.com/eosrio/hyperion-atomicassets-plugin/issues/50) | The original marketplace list needs complete kind enumeration, while the existing compound read is known-key only. An additive SDK implementation has native Node 24/26 and packed-consumer controls; review is resolving exact attempt accounting before publication. | Complete SDK review/CI, implement native MongoDB exhaustion and owner/request fences, then the actual marketplace list and paired reference HTTP comparison. |
| Atomic physical reference comparison | A separately pinned PostgreSQL/MongoDB comparison source is prepared. The frozen 19-kind fixture has not been silently retargeted to current 20-kind source. | Current custody/admission review, actual physical execution and managed HTTP serving acceptance. |

The current Atomic source has six bounded ID/name route components against 93 original JSON
paths; 87 paths have no current route. This is a static source inventory, not an endpoint pass
rate. Lists, filters, sorting, pagination, history, statistics, prices and socket families remain
substantial work. Full parity is still required.

## Wire failure attribution

The latest completed physical diagnostic replay remains FAILED at target 1340 with both last-polled
healthy projections at 1336 and last completed requested barrier 1308. Its master shutdown failure
remains separate from successful physical cleanup. Subsequent preparation attempts did not run a replay.

Independent recomputation of all 11,353 stage rows separates 313 mapping rejections at block 1244:
their source preparation totals 82.253 seconds. The earlier 87.453-second figure includes 44 successful
frames before that failure. Conservative joins attribute 31,152 GET attempts to 309 proof opens.
A small ABI-correct control reproduces the shared next-drain barrier. These awaited/nested timings
do not establish exclusive cause, CPU consumption, recoverable delay or a paired speedup.
No retry-policy or authority-cache optimization has been selected from this evidence.

## Remaining release gates

1. Full Wire/MongoDB replay and current-testnet sync, then restart, fork, drain, live and API serving acceptance.
2. Full AtomicAssets/AtomicMarket parity with actual same-input reference comparisons.
3. Integrated canonical recovery, finality/query gating and mandatory ABI-versioned declarative mapping reload.
4. Retained-history upgrades from 3.3.10-1 and 4.1.0, including the approved Elasticsearch 8.x/9.x paths for 7.x-created indices.
5. Stream-client compatibility, capacity/backpressure, runtime and pinned distribution qualification, plus operator release documentation.

Node 24 is the minimum, with maintained Node 24/26 checks; Bun 1.4.4 and TypeScript 7.0.2 remain the
selected checker toolchain. Checker success does not qualify Bun as the production indexing runtime.
The Wire explorer root-page deployment remains a roadmap item.

[Architecture](../architecture/hyperion-4.5.md) remains the release contract;
[release issue #24](https://github.com/eosrio/hyperion-tools/issues/24) coordinates owning-repository work.
No overall completion percentage, release date, deployment or completed release qualification is claimed.
