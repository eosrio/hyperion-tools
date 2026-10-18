---
status: accepted
date: 2026-10-16
---

# Serve reversible head state by default with an irreversible-only option per chain

Hyperion 4.5 state APIs default to the latest successfully processed canonical reversible head.
They must correctly roll back orphaned-branch effects and apply the replacement branch after a
fork. This includes affected projections, query indexes/derived values, and ABI/mapping selection.

Operators may select irreversible-only state serving per chain. That view must correspond to a
fully applied canonical position at or below LIB, with reversible changes excluded. All enabled
state-query domains on a chain use its selected mode.

Both MongoDB and WormDB must satisfy the selected mode's correctness requirements. The allowance
to release 4.5 with MongoDB alone does not relax fork correctness. Existing reversible-input paths
are implementation evidence, not qualification of complete state rollback.

Affected queries pause during fork recovery and return a retryable error when recovery exceeds
their request timeout; see [ADR 0016](0016-pause-affected-queries-during-fork-recovery.md).
Detailed visibility/error handling, confirmation depth, cross-tier consistency, mode changes,
and recovery mechanisms remain to be specified.
