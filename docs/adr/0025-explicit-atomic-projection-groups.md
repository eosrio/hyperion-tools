---
status: accepted
date: 2026-10-17
---

# Declare atomic projection groups as backend capabilities

Definitions/plugins explicitly declare when multiple projection mutations must commit together
all-or-nothing within the selected state backend. Core/runtime validates this requirement against
the backend and installed adapter before activation. Reject activation with a clear capability
error if the guarantee cannot be provided; do not silently weaken it.

Ordinary mutation batches preserve ordered, idempotent, recoverable application and durable
completion before acknowledgment. A batch does not automatically require a multi-record
transaction. Partial ordinary-batch application must remain safely recoverable, and incomplete
work must not be acknowledged as completed.

This permits live and bulk execution to choose batching strategies while preserving plugin-required
atomic groups. Backend-native transaction mechanisms remain adapter responsibilities; definitions
do not depend on MongoDB/WormDB transaction APIs.

Exact group boundaries, supported operations, read isolation, and checkpoint/recovery protocols
remain open. Atomic commit alone does not specify a consistent snapshot across several API reads.
The decision does not require transactions across independent projections or other stores.
