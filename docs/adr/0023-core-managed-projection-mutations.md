---
status: accepted
date: 2026-10-17
---

# Apply declarative and code-hook projection mutations through the shared runtime

Declarative mappings and projection hooks use the same managed-mutation model:

1. Plugins receive source events and read through backend-neutral interfaces.
2. Mapping execution/code hooks produce or stage batches of backend-neutral projection changes.
3. Core/runtime validates and coordinates application through the selected backend adapter,
   including idempotency, recoverable checkpoints, durable completion/acknowledgment, and rollback.

Plugins own domain semantics. Core/runtime owns the generic application/recovery lifecycle.
Backend adapters own native MongoDB/WormDB operations and physical optimization. Portable
projection hooks do not bypass that lifecycle with direct native database writes.

This applies the accepted recovery guarantees to both declarative and custom processing without
requiring a global writer, cross-store commit barrier, or database transaction per source event.
Projection ordering and declared parallelism are defined in
[ADR 0024](0024-ordered-projections-with-declared-parallelism.md). Exact logical operations, read-view
details, batch guarantees, hook runtime/syntax, and legacy-hook migration remain to be specified.
API handlers, protocol codecs, and notification effects have separate execution requirements.
