---
status: accepted target
date: 2026-10-16
---

# Include contract mappings and definitions in the 4.5 live-reload target

Hyperion 4.5 targets adding or updating declarative contract-indexing definitions and mappings
without restarting the indexer. Contract mappings are the operator's most common changes, so
operational-settings-only reload would miss the intended use case. Validate changes and identify
required backfill/rebuild work before exposing the new behavior. Exact activation and serving
semantics remain to be decided; executable hook or protocol-decoder changes may require restarts.

The operator subsequently made live reload of declarative contract mappings a mandatory 4.5 launch
requirement in [ADR 0038](0038-require-live-declarative-mapping-reload-for-45.md). Staged rebuild and
serving behavior are defined in
[ADR 0009](0009-bind-contract-mappings-to-abi-deployments.md).
