---
status: accepted
date: 2026-10-17
---

# Require the served view's block number for last-valid-state responses

If a plugin chooses to serve last-valid state after an isolated projection failure, every such
response must expose the block number through which that served view is complete for the declared
projection scope. The number must describe the data actually returned, including cached responses.
Current chain head, another consumer's progress, and per-row last-modification blocks are not
substitutes.

The view must actually remain available and compatible. A progress checkpoint alone does not
reconstruct an earlier valid view after partial writes. Keep the data and its progress marker
paired; an implementation unable to establish that pairing cannot claim this serving contract.
Known orphaned state remains subject to the accepted fork-recovery query pause.

The requirement supplies coverage information. It does not by itself promise a historical
single-block snapshot across multiple partitions/projections or change normal processing-lag
and staged-rebuild serving policies.

This decision establishes the mandatory block-number condition. Exact metadata field names,
additional provenance, and multi-projection/non-HTTP representations remain open. The default
failure-serving policy was subsequently accepted in
[ADR 0039](0039-default-to-retryable-unavailable-for-failed-projections.md).

Shared HTTP response headers were subsequently accepted as the metadata transport in
[ADR 0037](0037-shared-http-state-metadata-headers.md), preserving compatible JSON response bodies.
