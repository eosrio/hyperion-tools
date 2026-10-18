---
status: accepted
date: 2026-10-17
---

# Require projection initialization strategies and source-coverage declarations

Contract-indexing plugins must declare each projection's supported initialization strategies and
required source coverage. Hyperion verifies that available sources satisfy those requirements before
marking a projection ready. Operators should not have to infer the requirements from plugin internals.

A snapshot plus forward catch-up may suffice for a current-state projection. A projection that
depends on earlier history may require historical backfill or a separately qualified seed. Snapshot
contents, selected Elasticsearch history, and replayable source events are not interchangeable
without checking the inputs and coverage the projection actually needs.

Existing snapshot loading and state synchronization are mechanisms to extend and qualify, not proof
that every new projection can be initialized from them. Preserve the history-upgrade requirement in
[ADR 0040](0040-preserve-history-without-full-reindex-on-45-upgrade.md); state initialization does not
authorize requiring a full Elasticsearch history reindex.

Exact declaration formats, source interfaces, identity and coverage checks, snapshot-to-live handoff,
gap handling, and resumable job mechanisms remain to be specified.
