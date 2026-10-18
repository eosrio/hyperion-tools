---
status: accepted
date: 2026-10-17
---

# Share state-response metadata headers across HTTP plugins

Use one shared Hyperion HTTP header contract across plugins for the mandatory block number of a
last-valid view and an explicit last-valid-state indicator. Preserve compatible JSON response
bodies, including the pinned AtomicAssets/AtomicMarket formats. Plugins must not invent incompatible
metadata representations for this shared behavior.

The shared API host/contract must make the headers readable by browser clients. Bind the block
marker to the actual returned view, including cache hits; do not replace it with current chain or
consumer progress. The last-valid-state indicator must accurately reflect the serving mode.

Exact header names, additional provenance such as block ID, multi-projection responses, and
non-HTTP representations remain to be specified. This decision does not settle the default
unavailability/last-valid-state policy for failed projections or change the accepted snapshot,
normal-lag, staged-rebuild, and fork-recovery contracts. The default failure-serving policy was
subsequently accepted in
[ADR 0039](0039-default-to-retryable-unavailable-for-failed-projections.md).
