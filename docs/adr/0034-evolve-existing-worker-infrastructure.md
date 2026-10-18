---
status: accepted
date: 2026-10-17
---

# Evolve Hyperion's existing worker infrastructure

Use the existing master/worker infrastructure as the 4.5 execution baseline. The operator reports
that it has been working well and supports improvements. Source inspection confirms separate
live/history worker roles, contract-usage-based pool assignments, worker relaunch, queue
backpressure, and bounded consumer autoscaling.

Build on these mechanisms for the accepted projection ordering, backend-neutral hook execution,
durable progress, replay/fork handling, and contract-failure isolation contracts. Distinguish
existing behavior from new or stronger guarantees that still need implementation/qualification.

The exact hook-worker layout remains open. The operator's direction does not settle the earlier
separate-hook-pool proposal or require a process per plugin/contract. Justify changes to process/
queue boundaries with representative isolation, throughput, and recovery evidence while retaining
the distinct live and historical execution strategies.

Job/IPC boundaries, scheduling/resource budgets, timeout/supervision, and restart scope remain to
be defined. Source observations and the operator's production experience are not substitutes for
qualification of new execution guarantees.
