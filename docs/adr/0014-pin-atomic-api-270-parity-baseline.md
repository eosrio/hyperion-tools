---
status: accepted
date: 2026-10-16
---

# Pin the AtomicAssets and AtomicMarket parity reference to API 2.7.0

Hyperion 4.5's full AtomicAssets and AtomicMarket API parity requirement uses
[`atomicassets/atomicassets-api` release 2.7.0](https://github.com/atomicassets/atomicassets-api/releases/tag/2.7.0)
at exact commit
[`0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e`](https://github.com/atomicassets/atomicassets-api/commit/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e)
as its reference.

Pinning the immutable source commit gives all implementation agents and conformance checks the
same target. Moving branches, local checkouts at other revisions, and later upstream releases do
not silently change that target. A baseline change must be recorded explicitly and its affected
compatibility obligations requalified.

This pin governs the AtomicAssets and AtomicMarket API surfaces under ADR 0013. Supported
chain/contract deployments and the detailed conformance inventory remain separate decisions.
