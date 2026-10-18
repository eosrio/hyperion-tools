---
status: accepted
date: 2026-10-17
---

# Require live reload of declarative contract mappings for 4.5

Live reload of declarative contract mappings is mandatory for the Hyperion 4.5 launch. Operators
must be able to add or update mappings without restarting the indexer. This makes the target in
[ADR 0008](0008-live-reload-contract-mappings-and-definitions.md) a launch requirement for mappings,
including if 4.5 launches with MongoDB alone.

Validate changes before activation. Changes that require backfill or rebuilding follow the accepted
staged replacement workflow; accepting a new mapping does not imply existing data has been rebuilt.
The same declarative mappings remain portable across the supported state backends.

Executable hooks and native protocol components may still require controlled restarts. This decision
does not require live replacement of executable/native code or settle other runtime configuration
reload categories. Exact coordination and activation mechanisms remain to be specified.
