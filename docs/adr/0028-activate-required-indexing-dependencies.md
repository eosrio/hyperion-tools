---
status: accepted
date: 2026-10-17
---

# Activate required indexing dependencies when enabling a plugin

Plugins declare required projections/query contracts and compatible versions. When an operator
enables a plugin for a chain, core/runtime resolves and activates its required indexing dependencies
automatically within the configured contract scope. Dependencies must not silently expand an
account allowlist.

Requiring another plugin's indexed data does not automatically expose that plugin's API routes.
Validate compatible installed versions and backend capabilities before activation, and satisfy
prerequisite readiness before dependent functionality serves requests. Missing or incompatible
prerequisites reject the dependent activation with an actionable explanation; unrelated indexing
continues. This does not introduce a common-block barrier across projections.

The operator accepted this general rule while clarifying that AtomicAssets and AtomicMarket are
one operator-facing plugin for simplicity; see
[ADR 0029](0029-combined-atomicassets-atomicmarket-plugin.md). The dependency example does not
require splitting those domains into separately managed plugins.

Dependency declaration syntax, optional dependencies, cycles, resolved-plan inspection, disabling/
upgrading prerequisites, and detailed readiness/error contracts remain open.
