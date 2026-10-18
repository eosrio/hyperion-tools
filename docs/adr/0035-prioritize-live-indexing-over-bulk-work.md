---
status: accepted
date: 2026-10-17
---

# Prioritize live indexing over historical work by default

When live indexing and historical backfill compete for configured resources, live indexing has
priority by default. Protect live-processing capacity and throttle bulk work first under contention.
Historical work can use spare capacity; a bulk-only profile can use its full configured budget.
Retain the distinct live and historical execution strategies.

Build on existing live/history worker roles and queues. Those mechanisms alone do not establish
resource reservation across CPU, memory, storage, or shared pools; qualify the chosen scheduling
and admission policy with representative combined workloads.

Infrastructure backpressure and queue capacity limits remain applicable. Contract-specific failures
still require isolated recovery. This decision does not guarantee a fixed latency under insufficient
capacity or select exact resource budgets, reservation/borrowing rules, or operator override syntax.
