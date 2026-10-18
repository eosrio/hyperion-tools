---
status: accepted
date: 2026-10-17
---

# Declare snapshot consistency per endpoint

Plugins explicitly require a shared read snapshot for endpoints whose related queries must use
one stable database view. For example, matching-row counts and paged results can share that view
while indexing continues. Ordinary reads use the latest eligible state without an automatic
snapshot spanning all queries in every request.

The requirement belongs to the endpoint/query contract, not routine operator tuning. Validate
required snapshot support against the selected backend/adapter before activation. Do not silently
downgrade a declared consistency guarantee.

This is the current per-endpoint policy; broader consistency defaults may be improved later.
It does not require shared snapshots between HTTP requests, common chain progress across
independent projections, or a state/history block barrier. Existing finality, fork-recovery,
and readiness requirements still apply.

Read-view lifetime, resource limits, error/retry behavior, mapping-generation pinning, and
backend qualification remain open.
