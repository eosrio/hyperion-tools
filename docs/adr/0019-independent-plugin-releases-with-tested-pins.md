---
status: accepted
date: 2026-10-17
---

# Release first-party plugins independently with compatible versions pinned by Hyperion

EOS Rio-maintained first-party plugin packages have versions and releases independent of Hyperion
core. Each Hyperion release identifies tested, pinned compatible plugin versions, providing
operators with a supported combination without manual compatibility management.

Domain fixes may ship independently of a Hyperion core release. Applicable core/plugin interface,
backend capability, mapping activation, and API parity obligations still apply. Independent
versioning does not bypass those checks or require core and plugin versions to advance together.

Standard distributions include the pinned plugins, disabled by default and enabled per chain
through configuration; see [ADR 0020](0020-bundle-pinned-plugins-with-per-chain-opt-in.md).
Exact packaging, repository locations, the compatibility-manifest format, and install/upgrade
behavior remain to be specified. Automatic compatibility selection does not by itself authorize
unattended runtime updates.

Operator-initiated executable plugin upgrades and an official repository catalog containing
compatibility declarations were subsequently accepted in
[ADR 0046](0046-operator-initiated-plugin-upgrades-and-repository-catalog.md).

Packaging clarified in [ADR 0029](0029-combined-atomicassets-atomicmarket-plugin.md): AtomicAssets and
AtomicMarket share one plugin package, version, and release; their versions are not managed separately.
