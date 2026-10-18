---
status: accepted
date: 2026-10-16
---

# Retain RabbitMQ as the default ingestion transport for Hyperion 4.5

Hyperion 4.5 keeps RabbitMQ as its default ingestion transport, building on the reader,
deserializer, and indexing-worker queues already present in Hyperion. Integrating the selected
state backend and contract-indexing improvements should use that existing transport.

An explicit transport interface is useful follow-up work. Its extraction is not an additional
4.5 launch gate, and removing RabbitMQ or delivering an alternate transport is not required by
this decision.

Detailed event, delivery/acknowledgment, durability/replay, and module ownership contracts remain
to be defined. RabbitMQ selection alone does not qualify those guarantees.
