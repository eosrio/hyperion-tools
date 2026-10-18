---
status: accepted
date: 2026-10-17
---

# Let the shared Hyperion runtime orchestrate initialization and rebuilds

The shared Hyperion runtime owns projection initialization, backfill, and rebuild job scheduling,
durable progress/checkpoints, retry/resume, and readiness publication. Plugins declare their input,
coverage, dependency, and initialization requirements and supply domain-specific transformations
through shared contracts. They do not each need an independent job/recovery infrastructure.

Build on Hyperion's existing workers and tools, preserving live indexing priority and isolation of
contract-specific failures. Apply the accepted backend-neutral projection execution, staged rebuild,
and source-coverage/readiness contracts to these jobs.

This decision assigns responsibility without selecting a new worker pool or requiring a separate
orchestration service. Exact job interfaces, checkpoint mechanics, scheduling, and process placement
remain to be specified. It does not introduce a global state/history commit barrier.
