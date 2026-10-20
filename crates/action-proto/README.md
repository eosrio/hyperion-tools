# action-proto — direct-from-disk action reader (experimental)

Part of the prototype read path for the next-gen indexer (alongside
[`delta-proto`](../delta-proto)): decode Hyperion-shaped **action** documents straight from the on-disk
logs, at core-scaling throughput, with no `ship-0` serializer in the loop.

`action-proto` decodes `action_traces` from `trace_history` into Hyperion-shaped action NDJSON (or straight
to Elasticsearch), and supports a **cold-tier metadata-only** mode that omits the heavy payload the
[`archive-server`](../archive-server) re-serves on demand.

```bash
action-proto --from-disk /data/nodeos/state-history --abi-index abi.ndjson \
  --start 2 --end 999999999 --threads 12 --out actions.ndjson
```

For 4.5, root and receipt uint64 sequence fields are canonical decimal JSON strings, even below
2^53. `_id` continues to use the exact decimal sequence, and frozen binary input remains unchanged.
ABI/code uint32 counters, block numbers and ordinals stay numeric. See the
[sequence JSON checks](../../docs/qualification/lossless-sequence-json.md) for boundary coverage and
the separate Elasticsearch/Wire qualification obligations.
