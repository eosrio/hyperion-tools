# range-server — the disk range service (Wire)

A long-running, read-only process next to a Wire node's `state-history` and `blocks` directories.
Hyperion's master **pulls** sealed block ranges from it: Hyperion 4.5 block, action and delta documents,
as Elasticsearch `_bulk` lines grouped per block, followed by a range manifest the master can check
before it writes a range checkpoint. The service never pushes, never writes to Elasticsearch and
never needs its credentials.

```bash
range-server --protocol wire-sysio-6508c9e8-engineering-v1 \
  --state-history /srv/wire/state-history --blocks-dir /srv/wire/blocks \
  --catalog-dir /srv/wire/catalogs --genesis-catalog-sha256 <genesis manifest sha256> \
  --chain-id <chain id> --doc-profile-id wire-sysio-6508c9e8-native-engineering-v1 \
  --listen 127.0.0.1:7180 --threads 16 --seal --ship-abi wire-ship-schema.json
```

## What it serves, and what it refuses

- **The sealed LIB.** ABI history catalogs (from `abi-scanner`) are found under `--catalog-dir`. The genesis
  catalog is pinned by its manifest's SHA-256; every extension is verified against its base and the
  longest verified chain is served. The sealed LIB is the newest catalog's end, bounded by what the logs
  hold. A range whose `to` is past it is refused (`409 beyond_sealed_lib`).
- **Catalog verification** applies the checks the 4.5 API applies to a pinned catalog (bounded manifest;
  chain, node revision, profile and SHiP schema identity; irreversible coverage; member sizes and
  digests; every record's shape, position and block id), plus what makes a segment an extension: it
  names its base's digest, end and last id; its new records start at the base's end + 1; the base's
  records and block ids are a byte prefix of its own; its global sequence does not go back. Member files
  are pinned by identity and revalidated before every range.
- **Every served block is a catalog member.** Its trace-log, chain-state and block-log ids must equal the
  catalog's id for it, its header must hash to that id, and it must name the previous block's id. Any
  disagreement fails the range.
- **Sealing (`--seal`).** The service extends the chain itself, as segments that hold only the
  blocks they add (`hyperion-abi-history-segment-v1`, see `abi-scanner --segment`):
  - It seals once `--seal-blocks` irreversible blocks (default 120) follow the chain, or once
    the oldest unsealed block has waited `--seal-max-wait-ms` (default 60 s).
  - The end block is the block log's last complete block. The block log holds irreversible
    blocks only, so the chain can never fork away from it.
  - Every new block's header must hash to the id the state-history logs give it and name the
    previous block's id.
  - The segment is loaded back through the consumer verifier before it is served.
  - A seal reads only the new blocks.
- **Follow mode.** The logs' indexes are re-measured on every request, so a running node's growing logs
  are followed; `blocks.log` holds irreversible blocks only, and a half-written last block is never read.
  With `--follow`, extensions added to `--catalog-dir` (by `abi-scanner --base-manifest`) are verified and
  adopted while the service runs, which moves the sealed LIB forward.
- **Documents** are exactly what `action-proto` and `delta-proto` write, from the same producers: blocks
  with `protocol`, schema-3 action occurrences, and deltas whose `abiDeployment` is the catalog epoch.
  The same request over the same catalog chain returns the same bytes.
- `abis` is refused (`400 unsupported_kind`) until a producer of 4.5 ABI documents has parity evidence.

## API

| endpoint | purpose |
|---|---|
| `GET /v1/info` | chain id, profile, node revision, SHiP ABI hash, genesis and newest catalog, sealed LIB, LIB, head, log ranges, limits, and the sealer's state with `--seal` |
| `GET /v1/catalog?after=<manifest sha>` | the verified catalog segments after that one (all of them without `after`) |
| `GET /v1/catalog/<manifest sha>/<name>` | a segment's manifest or member file, verbatim, for consumers that verify it themselves |
| `POST /v1/ranges` | stream a range (below) |
| `POST /v1/ranges/<stream>/credits` | `{"frames": N}` grants a credited stream N more block frames |
| `DELETE /v1/ranges/<stream>` | cancel a credited stream |
| `GET /v1/health` | liveness |

A range request:

```json
{"from": 1, "to": 50000, "kinds": ["blocks", "actions", "deltas"],
 "profile": "wire-sysio-6508c9e8-engineering-v1", "docProfile": "wire-sysio-6508c9e8-native-engineering-v1",
 "index": {"prefix": "wire", "version": "v1", "partitionSize": 10000000},
 "credits": 64, "threads": 8, "indexTransferMemo": false}
```

`profile` must be the service's protocol (`409 profile_mismatch` otherwise). `kinds` defaults to all three,
`docProfile` and `index` to the service's defaults. `threads` asks for fewer workers than `--range-threads`;
all ranges share `--threads`.

### Response: framed NDJSON

`200`, `Transfer-Encoding: chunked`, `X-Range-Stream: <id>`, `X-Range-Catalog: <sha>`. Every frame starts with
one JSON header line:

```text
{"frame":"start","from":1,"to":50000,"kinds":[...],"catalogSha":"…","previousBlockId":null}
{"frame":"block","block":1,"id":"…","bytes":418,"docs":{"blocks":[1,418],"actions":[0,0],"deltas":[0,0]}}
<418 bytes: the block's _bulk lines>
…
{"frame":"manifest","from":1,"to":50000,"blocks":50000,"catalogSha":"…","previousBlockId":null,
 "firstBlockId":"…","lastBlockId":"…",
 "kinds":{"blocks":{"docs":…,"bytes":…,"sha256":"…"},"actions":{…},"deltas":{…}},
 "firstGlobalSequence":"1","lastGlobalSequence":"…","skipped":{"actionsUndecoded":0,"deltaRows":{…}}}
```

- A `block` header is followed by exactly `bytes` bytes: an index line and a document line per document,
  kinds in the order blocks, actions, deltas. `docs` gives each requested kind's `[documents, bytes]`, so a
  client can route or hash each kind without parsing documents.
- The `manifest` trailer gives per kind the document count, byte count and the SHA-256 of that kind's
  `_bulk` bytes in stream order; the first and last block ids (`previousBlockId` chains ranges); the first
  and last committed global sequence; and what the producers left without a document, as Hyperion does
  (undecodable action data is kept raw; delta rows without an ABI table get no document).
  `catalogSha` is the catalog that sealed `to`.
- A response without a manifest is incomplete. A failure after the first byte ends the stream with
  `{"frame":"error","message":…}`.

### Backpressure

Each range is cut into 512-block chunks that worker threads take one at a time from a shared cursor; one
fold streams them in block order, and no worker runs more than two chunks past the busy ones. So a
slow client slows the readers instead of growing memory:

- without `credits`, the socket is the limit (TCP backpressure);
- with `credits: N`, the service sends N block frames and then waits for `POST /v1/ranges/<stream>/credits`;
  a stream left without credit for `--credit-timeout` seconds fails.

## Reading the logs

Each chunk is read as one slab per log: one positional read of its index slots and one of its byte
region, instead of several small reads per block. Blocks are cut out of memory by their index slots, so a
superseded fork entry between two canonical ones is never parsed. The same chunked reader drives
`action-proto` and `delta-proto`, whose output is now written in block order.
