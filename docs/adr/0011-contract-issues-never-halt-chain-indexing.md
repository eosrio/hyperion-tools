---
status: accepted
date: 2026-10-16
---

# Contract issues never halt chain indexing

Operators do not control the smart contracts being indexed. Contract ABI changes, missing or
incompatible mappings, and contract-specific processing failures must therefore be isolated from
shared source capture and unrelated indexing. Preserve replayable inputs and per-projection failure
positions so affected projections can recover without pretending that skipped processing completed.
Detailed failure isolation, retention, and query behavior remain to be defined.

The recovery-retention policy was subsequently accepted in
[ADR 0043](0043-bound-recovery-retention-and-report-unavailable-inputs.md): use a configurable bounded
local recovery window and historical sources for older inputs; unavailable required data leaves the
affected projection incomplete while unrelated indexing continues. Query behavior is defined in
[ADR 0039](0039-default-to-retryable-unavailable-for-failed-projections.md).
