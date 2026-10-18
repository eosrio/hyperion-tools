---
status: accepted
date: 2026-10-17
---

# Bound local recovery retention and report unavailable inputs

Use a configurable bounded local recovery window for isolated projection failures. Use configured
historical sources for older inputs, checking that their actual data and coverage satisfy the
projection's declared requirements. A contract that remains unprocessable must not accumulate
unbounded local data or eventually halt unrelated chain indexing.

If required inputs are unavailable from configured sources, mark the affected projection incomplete
and requiring source data/backfill or a qualified reinitialization strategy. Keep unrelated indexing
running. Do not silently skip missing inputs, advance completed progress past a gap, or claim
readiness. Preserve failure context and processing positions; checkpoints do not prove source data
still exists.

Affected queries follow the accepted default retryable-unavailable policy, with last-valid-state
serving only where explicitly supported. This recovery policy does not replace infrastructure
backpressure or weaken the required fork-rollback guarantees.

Exact budgets, expiration rules, storage mechanisms, source selection, and operator visibility remain
to be specified. This decision does not mandate a new buffer service or select a physical layout.
