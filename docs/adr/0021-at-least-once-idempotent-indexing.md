---
status: accepted
date: 2026-10-17
---

# Use at-least-once delivery with idempotent indexing and durable completion before acknowledgment

Indexing consumers may receive the same source event more than once. Acknowledge a message/batch
only after its required effects are durably stored and restart progress is recoverable. Redelivery
after a crash must not double-count derived values or regress state. Progress must not skip failed
or partially applied work.

State/history consumers retain independent progress; this does not introduce a cross-store commit
barrier. Backend-specific batching, durability, deduplication, and checkpoint mechanisms may differ
while preserving the contract. Contract-specific failures remain isolated under ADR 0011.

The current Mongo dynamic-table path invokes its callback from `Promise.all(...).finally(...)`,
and the indexer callback calls `ackAll`. Rejected writes therefore reach the acknowledgment path.
Record this source-confirmed bug for correction and regression qualification under this contract.

Exact identities, ordering, durability levels, partial-failure handling, and source replay coverage
remain to be specified. This decision does not prescribe a distributed transaction or per-event fsync.
