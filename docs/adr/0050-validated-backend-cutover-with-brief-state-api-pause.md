---
status: accepted
date: 2026-10-17
---

# Validate and catch up the destination before backend cutover

Backend changes are operator-initiated. Initialize, catch up, and validate destination state
projections before an explicit cutover. Hyperion 4.5 may briefly pause the affected chain's state
API during that cutover, provided the destination is caught up and validated before switching.

All enabled state domains continue using one active backend per chain. A destination being staged
for migration is not a second active serving backend. Keep existing Elasticsearch history usable
under the accepted history-preserving upgrade contract.

Exact write routing, catch-up positions, validation evidence, in-flight requests, checkpoints,
rollback, and migration tooling remain to be defined. This decision does not establish an implemented
migration tool, require a zero-downtime cutover, or require both adapters to ship at the initial 4.5
launch.
