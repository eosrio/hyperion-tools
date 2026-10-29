---
status: accepted
date: 2026-10-15
updated: 2026-10-29
---

# Launch Hyperion 4.5 with MongoDB for state and Elasticsearch for history

The architecture targets permanently supported MongoDB and WormDB alternatives with portable
contract-indexing definitions. On 2026-10-29 the operator selected MongoDB as the state backend
for the 4.5 launch. Elasticsearch continues to store and serve history.

WormDB implementation and qualification are follow-up work and do not block 4.5. This resolves
the earlier conditional MongoDB-only allowance; no effort threshold or further backend launch
decision is required. Preserve backend-neutral definitions and interfaces. Any WormDB adapter
offered as supported later must satisfy the applicable capability, API, finality and recovery
contracts.

Full AtomicAssets and AtomicMarket API parity remains required for 4.5, including a MongoDB-only
launch; see [ADR 0013](0013-require-atomicassets-atomicmarket-parity-for-45.md).
