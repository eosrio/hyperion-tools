---
status: accepted
date: 2026-10-17
---

# Establish repository homes and coordinate the 4.5 release

Use eosrio/hyperion-tools as the release coordination home, with substantial worker/API integration
in eosrio/hyperion-history-api and engine-specific work in igorls/wormdb.

Create eosrio/hyperion-sdk for the independently versioned extension contracts and authoring tools,
eosrio/hyperion-atomicassets-plugin for the combined AtomicAssets/AtomicMarket operator package,
and eosrio/hyperion-wire-plugin for the coordinated Wire operator package.

Reuse eosrio/hyperion-plugins as the official catalog. Plan migration from the existing
eosrio/hyperion-plugin-core dependency explicitly; do not silently replace existing consumers
or imply that legacy catalog entries have been qualified for 4.5.

Keep one release coordination issue, with implementation issues in the owning repositories and
real dependency links. The approved first batch is the starting frontier, not the complete release
backlog. Backend-adapter source homes and detailed runtime placement remain foundation work.

Repository bootstrap, documentation and CI do not establish SDK or plugin implementation,
API parity, backend qualification or release readiness.

See the [roadmap](../roadmap/hyperion-4.5.md) and [initial batch](../roadmap/hyperion-4.5-initial-issues.md).
