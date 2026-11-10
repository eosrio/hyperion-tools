# delta-proto — direct-from-disk delta reader (experimental)

Part of the prototype read path for the next-gen indexer (alongside
[`action-proto`](../action-proto)): decode Hyperion-shaped **delta** documents straight from the on-disk
logs, at core-scaling throughput, with no `ship-0` serializer in the loop.

`delta-proto` decodes `contract_row` table deltas from `chain_state_history` into Hyperion-shaped delta
NDJSON, and supports a **cold-tier metadata-only** mode that omits the heavy payload the
[`archive-server`](../archive-server) re-serves on demand.

```bash
delta-proto --from-disk /data/nodeos/state-history --abi-index abi.ndjson \
  --start 2 --end 999999999 --threads 12 --out deltas.ndjson
```

The Wire path (`--protocol wire-…`) cuts a run into 2,048-block chunks that worker threads take one at a
time, reads each chunk as one slab per log, and writes documents in block order. Its per-block producer
(`wire::DeltaWorker`) is the crate's library, which [`range-server`](../range-server) drives the same way.
