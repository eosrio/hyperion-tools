---
status: accepted
date: 2026-10-17
---

# Ship AtomicAssets and AtomicMarket as one AtomicAssets plugin

Provide one operator-facing AtomicAssets plugin containing both AtomicAssets and AtomicMarket
indexing and APIs. The community treats the AtomicAssets API as one system, including AtomicMarket.
Operators manage one package, version, and release. Internal modules remain separate where useful
and participate in the appropriate indexing/API runtime lifecycles.

The combined plugin is maintained by EOS Rio, releases independently of Hyperion core, and is
pinned as one tested compatible version for each Hyperion release. It is bundled in standard 4.5
distributions, disabled by default, and enabled per chain through configuration. Full parity for
both API domains against the pinned baseline remains required.

This clarifies the plural wording in ADRs 0018–0020 and the dependency example in ADR 0028; none
requires separate AtomicAssets and AtomicMarket packages or release versions. The generic plugin
dependency system remains applicable to other relationships and internal prerequisites.

Detailed configuration, optional internal API controls, distribution formats, and repository/module
locations remain open. Operators must not be required to install or coordinate a separate
AtomicMarket plugin to obtain the agreed AtomicAssets API system.
