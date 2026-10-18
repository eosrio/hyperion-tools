---
status: accepted
date: 2026-10-17
---

# Use JavaScript execution with TypeScript authoring for projection hooks by default

The Hyperion 4.5 default for executable contract projection hooks is JavaScript execution in the
Node.js runtime, with TypeScript authoring through the shared SDK. Build on Hyperion's current
plugin authoring/loading model while applying the accepted managed execution contracts.

Hooks consume normalized source events and shared read/query context, and produce/stage logical
mutations for runtime-managed application. Native database operations stay inside adapters, so
the same hook definitions work with MongoDB and WormDB. Ordering, idempotency, declared atomic
groups, durability, and fork/replay requirements remain applicable.

This decision concerns contract projection hooks. Chain protocol providers, native decoding/
acceleration, and backend implementations retain their separate interfaces and language choices.
A unified chain package can contain those components alongside the JavaScript/TypeScript modules.

Worker/process isolation, resource limits, supervision, hook replacement, native bindings, and
exact SDK/build/version contracts remain open. The decision does not require every extension
component to execute as JavaScript.
