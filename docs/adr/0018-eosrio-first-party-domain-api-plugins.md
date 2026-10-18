---
status: accepted
date: 2026-10-16
---

# Maintain AtomicAssets and AtomicMarket through first-party EOS Rio plugin support

AtomicAssets and AtomicMarket are first-class API domains maintained by EOS Rio and hosted by
Hyperion's existing Fastify API server through its plugin architecture. Keep domain-specific routes/behavior in the plugin,
using shared host facilities, backend-neutral state/query interfaces, and the agreed indexing
extension interfaces.

These are officially maintained 4.5 components subject to full API parity against the pinned
reference and qualification of the supported backend combinations. Existing plugin/API hosting
mechanisms provide the starting point.

This hosting decision does not prescribe the implementation language/process of every decoder,
query engine, or backend component. Independent plugin releases with tested compatible versions
pinned for each Hyperion release are defined in
[ADR 0019](0019-independent-plugin-releases-with-tested-pins.md). Distribution, repository locations,
and detailed module ownership remain to be defined.

Packaging clarified on 2026-10-17 in
[ADR 0029](0029-combined-atomicassets-atomicmarket-plugin.md): one AtomicAssets plugin contains both
AtomicAssets and AtomicMarket indexing and APIs. Earlier plural wording does not require separate
operator-facing packages.
