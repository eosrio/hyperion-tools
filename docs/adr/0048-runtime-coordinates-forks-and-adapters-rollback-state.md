---
status: accepted
date: 2026-10-17
---

# Coordinate fork recovery in the runtime and implement rollback in adapters

The shared Hyperion runtime coordinates state-projection fork recovery: canonical-branch handling,
affected projection identification, query gating, rollback coordination, and replacement-branch
application. Backend adapters implement rollback of stored projection effects behind the shared
contract. Plugins continue producing backend-neutral logical mutations.

This assigns recovery ownership without choosing an undo-log, before-image, multi-version, or other
physical storage mechanism. Protocol adapters retain their chain interpretation role. Stored
projection effects participate in the generic recovery contract; external notifications and other
side effects have separate requirements.

Preserve the accepted query pause during recovery, durable/recoverable progress, mapping/ABI binding,
and backend qualification requirements. The MongoDB-only launch allowance does not waive correct
fork recovery.

Exact block/checkpoint identities, operation signatures, publication barriers, and crash recovery
within a fork remain to be defined and qualified.
