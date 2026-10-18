---
status: accepted
date: 2026-10-17
---

# Release backend adapters as independently versioned extensions

State backend adapters use a dedicated extension interface and release independently of Hyperion
core. Each Hyperion release pins qualified adapter versions, giving operators a tested combination
without manually resolving adapter/core compatibility.

MongoDB and WormDB adapters follow this model. Operators continue selecting one active state
backend per chain. The adapter package version is separate from the database engine version;
portable plugin definitions and queries keep their shared runtime/adapter contracts.

Exact packaging, SDK boundaries, catalog metadata, and repository ownership remain to be specified.
This does not select a process layout, implementation language, native binding, or storage algorithm.
The MongoDB-only launch allowance remains in force; an unqualified WormDB adapter need not ship
with the initial 4.5 release.
