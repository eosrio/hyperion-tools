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

## Wire: Hyperion 4.5 documents

With a Wire `--protocol` and `--blocks-dir`, every block-log entry is walked once. Its header must hash
to the block id the trace log names, and each header must name the previous block's id, so a run fails
unless the two logs agree over its whole range. Actions are grouped by input origin, as the 4.5 indexer
groups them.

- `--chain-id <64 hex>` adds schema-3 occurrence metadata to every action document:
  `{"schemaVersion":3,"source":{chainId,blockNum,blockId},"selectionId":"h45abidiskselection1:…","input":{…}}`.
  The selection is the deterministic disk selection of the block's catalog position. A 4.5 API that
  proves catalog-covered history from the node's logs resolves it into exactly the metadata the indexer
  would have stored.
- `--blocks` also writes `<chain>-block-<version>` documents (`_id` = block number), including blocks
  before the trace log's first entry. `--doc-profile-id` sets their `protocol.profileId`. With `--out`,
  block documents go to `--blocks-out`.

```bash
action-proto --protocol wire-sysio-6508c9e8-engineering-v1 \
  --from-disk /srv/wire/state-history --blocks-dir /srv/wire/blocks --abi-index abi-history.ndjson \
  --chain-id <chain id> --blocks --doc-profile-id wire-sysio-6508c9e8-native-engineering-v1 \
  --index-transfer-memo --start 1 --end 999999999 --threads 16 \
  --es http://127.0.0.1:9200 --chain wire --index-version v1
```

Any failure (a log disagreement, an invalid action ancestry, a failed `_bulk` request) is reported and
makes the run exit nonzero.

## Reading and ordering

A run is cut into 2,048-block chunks that worker threads take one at a time from a shared cursor. Each
chunk is read as one slab per log (one read of its index slots, one of its bytes) and its blocks are cut
out of memory by their index slots, so superseded fork entries are never parsed. One fold hands the
chunks' documents to the sink in block order: `--out` and `--blocks-out` list blocks in order, and two
runs over the same range write the same bytes. The per-block producer is the crate's library
(`BlockWorker`), which [`range-server`](../range-server) drives the same way.
