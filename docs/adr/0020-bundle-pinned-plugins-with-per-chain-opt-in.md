---
status: accepted
date: 2026-10-17
---

# Bundle pinned first-party plugins and enable them per chain through configuration

Standard Hyperion 4.5 distributions include the tested, pinned AtomicAssets plugin, including
AtomicMarket, maintained by EOS Rio. It is disabled by default. Operators enable it per chain
through configuration, without separately installing plugins or matching compatible versions.

Bundling preserves independent plugin releases from Hyperion core and the tested compatible set defined in
ADR 0019. It does not itself activate the domain routes or indexing work on a chain.

Exact source/build/image distribution formats, installation/update commands, and detailed activation
configuration remain to be defined. Capability validation, projection readiness, and existing
mapping/rebuild contracts still apply.

The combined AtomicAssets/AtomicMarket package boundary is clarified in
[ADR 0029](0029-combined-atomicassets-atomicmarket-plugin.md).
