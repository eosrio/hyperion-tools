---
status: accepted
date: 2026-10-17
---

# Express portable plugin queries through backend adapters

Plugins express reads through a shared structured logical query model so the same plugin queries
work with MongoDB and WormDB. The model covers filtering, field selection, sorting, pagination,
counts, and explicitly specified lookup/aggregation operations required by supported plugins.
The precise operation vocabulary and semantics remain to be defined.

Plugins own logical query composition and domain response shaping. Native database query
construction remains inside backend adapters, which translate and optimize execution while
preserving the declared query behavior. Required query capabilities are checked before activation,
alongside indexing/write capabilities; unsupported requirements are not silently weakened.

Adapters may use different physical execution plans, including suitable materialized projections.
This decision does not require an unrestricted database language or support for every native
backend feature. It does not by itself change public request formats or weaken required API
compatibility, including the pinned AtomicAssets/AtomicMarket parity baseline.

Types/comparison semantics, read isolation, pagination/count contracts, cross-tier plans, and
capability/version syntax remain open.
