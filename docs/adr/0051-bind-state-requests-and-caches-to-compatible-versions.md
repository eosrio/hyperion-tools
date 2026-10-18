---
status: accepted
date: 2026-10-17
---

# Bind state requests and cached results to compatible backend/projection versions

Each state request uses one selected backend and a compatible set of required projection versions
for its lifetime. Complete using that context or return an appropriate retryable error if it becomes
invalid. Backend changes and mapping activation must not mix versions inside one response.

Bind cached results to the backend/projection versions that produced them. Known orphaned state
remains subject to fork-recovery query gating. The accepted per-endpoint snapshot policy and
independent state/history progress remain in force.

Exact context handles, cache keys, draining/retry, and old-generation retirement are implementation
design details.
