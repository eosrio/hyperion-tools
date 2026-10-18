---
status: accepted
date: 2026-10-17
---

# Preserve projection order and parallelize explicitly independent work

Core/runtime preserves event order per projection by default. Hooks account for earlier staged
changes in their ordered processing unit. Plugins may explicitly declare independent partitions
for parallel execution, preserving order within each partition. Physical optimizations must
preserve the declared logical results.

During live reading, independent contracts in the same block may execute concurrently. Respect
declared cross-contract dependencies, and preserve contract-failure isolation. This does not
require a global serial writer or a shared read barrier across client APIs.

Historical bulk filling may use advanced parallelization, batching, and reconstruction strategies
under a separate execution profile. Published results must meet the same ordering, idempotency,
mapping-version, and API contracts. Do not require bulk scheduling to copy the live implementation.

Exact event ordering, partition/dependency declarations, read-view details, bulk algorithms, and
repartitioning/recovery mechanics remain to be defined.
