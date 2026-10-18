---
status: superseded by ADR-0003
date: 2026-10-15
---

# Select the state backend per API domain within a chain

Superseded by [ADR 0003](0003-select-state-backend-per-chain.md) on 2026-10-15. The operator chose
per-chain configuration to reduce overlapping state ownership and cross-backend query edge cases.
The original decision below is retained as history and must not guide new implementation.

Operators choose MongoDB or WormDB independently for each `(chain, API domain)` pair. This allows
AtomicAssets to use WormDB while other state queries on the same chain use MongoDB. We accept the
additional configuration and validation combinations so operators can select a backend for each
domain's workload rather than commit an entire chain to one database.
