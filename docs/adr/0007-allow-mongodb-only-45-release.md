---
status: accepted
date: 2026-10-15
---

# Allow a MongoDB-only 4.5 release if WormDB qualification requires too much effort

The architecture targets permanently supported MongoDB and WormDB alternatives with portable
contract-indexing definitions. The operator is willing to implement missing WormDB capabilities,
but Hyperion 4.5 may launch with MongoDB alone if that work requires too much effort. This keeps
WormDB development open without making its readiness a prerequisite for the release. The capability
inventory, qualification evidence, effort threshold, and decision point remain to be defined.

Full AtomicAssets and AtomicMarket API parity remains required for 4.5, including a MongoDB-only
launch; see [ADR 0013](0013-require-atomicassets-atomicmarket-parity-for-45.md).
