---
status: accepted
date: 2026-10-17
---

# Retain shared-reader backpressure for infrastructure outages

Keep Hyperion's existing queue backpressure for 4.5. When an Elasticsearch backlog reaches its
configured queue budget, pause the shared reader and resume when the queues drain to the resume
threshold. New state input may pause with that reader; independent normal-path state/history
progress does not imply unlimited outage buffering.

This applies to infrastructure/capacity failures. Contract-specific failures must remain isolated
and must not halt chain indexing, including indirectly through a blocked shared queue.

The existing queue monitor is the implementation baseline. Exact resource budgets, source replay
coverage during pauses, alerting, and operational/recovery details remain to be defined.
