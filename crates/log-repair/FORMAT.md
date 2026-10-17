# Formats and recovery contracts

The on-disk formats this tool reads, the files it writes, and the rules it enforces.
For the operator workflow, see [README.md](README.md).

## Upstream references

Reviewed against these primary sources on 2026-10-15:

- [Spring block_log.cpp](https://github.com/AntelopeIO/spring/blob/main/libraries/chain/block_log.cpp):
  v3 header, first block number, genesis/chain ID, all-ones header separator,
  u64 little-endian entry positions and trailers, pruning flag.
- [Spring state_history/log.hpp](https://github.com/AntelopeIO/spring/blob/main/libraries/state_history/include/eosio/state_history/log.hpp):
  `ship` name magic, version 0, pruning feature, 48-byte entry header, zlib payload,
  three payload-size preambles, absolute position trailer. The Leap 4+ preamble
  includes the uncompressed size. The zlib payload is compressed by every release before
  Spring and stored by Spring (see [payload encodings](#ship-payload-encodings-across-nodeos-versions)).
- [Spring state_history/abi.cpp](https://github.com/AntelopeIO/spring/blob/main/libraries/state_history/abi.cpp):
  SHIP v0 request/result variant ordering and optional result fields.
- [Hyperion Tools SHIP client](https://github.com/eosrio/hyperion-tools/blob/main/crates/core/src/ship.rs):
  v0 request stability, exclusive end, acknowledgement flow, full-state delta caveat.
- [Hyperion Tools slice-log](https://github.com/eosrio/hyperion-tools/tree/main/crates/slice-log):
  retained-range slicing and rebasing SHIP entry trailers.
- [Hyperion Tools portable snapshot reader](https://github.com/eosrio/hyperion-tools/blob/main/crates/snapshot-load/src/reader.rs):
  portable snapshot magic `0x30510550` and file format 1. Snapshot head extraction
  is intentionally not inferred from a filename or a version-specific state row.

## Target logs

**`blocks.log` (v3, non-pruned).** The header is `[version=3:u32][first_block:u32]`,
then the packed genesis state (when `first_block` is 1) or the 32-byte chain ID,
then 8 bytes of `0xff`. Each entry is a packed `signed_block` followed by its own
absolute position as a u64 trailer. `blocks.index` holds one u64 position per block.
A block ID is the SHA-256 of the FC-packed header prefix, with its first four bytes
replaced by the big-endian block number. The pruning flag and other versions are
refused.

**SHIP logs (v0, non-pruned).** The magic is `0xc35d500000000000`: the Antelope name
`ship`, with version and feature bits 0. Each entry is
`[magic:u64][block_id:32][payload_size:u64][payload][pos:u64]`, little-endian. The
payload is `[marker:u32][uncompressed_size:u64, when marker = 1][zlib bytes]`.
Marker 1 carries the extra size, marker 0 is the Leap 3 framing, and other values
encode the compressed size in legacy logs. Every read checks sizes, stream
completion, the Adler checksum, and full consumption of the compressed input, with a
256 MiB decompression ceiling. No state-history ABI semantics are guessed from a
damaged payload.

The intact `.index` is the only authority for where entries start. Every entry read
checks its trailer against its index position. A `blocks.log` entry ends at the next
index position. A SHIP entry ends where its own `payload_size` says, which can be
earlier.

**Orphaned fork entries (SHIP).** After a fork, nodeos appends the replacement entries
and leaves the abandoned ones in place. The bytes between an entry's end and the next
index position are then whole SHIP entries from the abandoned fork, and each trailer
names its own position. Nothing reads them: the index skips them. They are common on
fast-forking chains: one per 16 blocks in a 30,000-block Libre range (1,859 orphans),
and 44 in 500,000 EOS blocks.

- Reads check that the gap is made of such entries, and refuse anything else.
- A rebuild keeps the gap's original bytes: they must be readable, lie outside every
  damaged extent, and frame correctly.
- Damage that reaches orphaned entries cannot be repaired. Nothing outside the
  damaged log holds those bytes: not another node's log (its orphans differ), not a
  state-history endpoint, not a replay. `scan` reports it as not repairable when the
  entry's header is readable; otherwise `build` refuses once the rebuilt entry shows
  where the orphans begin.

## Damage geometry

- A **record** is the unit of failure and of writing. Its size is a power of two,
  512 B to 16 MiB: `--record-size`, or the file's `st_blksize` by default.
- An **extent** is a run of unreadable records `[offset, offset + len)`, both aligned
  to the record size. Extents in a plan are sorted, disjoint, and not adjacent.
  Adjacent records always merge into one extent.
- A **region** spans the blocks whose entries (trailers included) overlap at least
  one extent. Extents merge into one region when their block ranges overlap or touch,
  that is, when no complete readable entry separates them.
  - The entries just before and after the region are its **anchors**.
  - Both anchors, and every byte of the region's entries outside its extents, must be
    readable.
  - A region may not include the log's first or last entry, nor touch the file header.

`scan` reads the selected range in aligned 16 MiB chunks. A chunk that fails with
`EIO` or `ENODATA` is re-read one record at a time; any other error stops the scan.
A partial scan (`--damaged-block`, `--block-range`) follows unreadable records past
either end of its range, so every extent is complete. The scan stops after
`--max-bad-reads` failed reads.

## Sources

**`P2PBLK01` container.** All integers are little-endian:

```text
"P2PBLK01"                       8-byte magic
first: u32, last: u32            inclusive block range, last - first < 10000
repeat for n = first..=last:
  n: u32                         must equal the expected block number
  len: u32
  packed signed_block[len]       exact bytes as stored in blocks.log
```

No trailing data is allowed. Each header must parse, carry block number `n`, and
reference the previous entry's ID. Any producer can write this format: build checks
the chain against the target's anchors and every length against the target index.

**Block logs.** An intact `blocks.log` with its index. Only entries are used; header
offsets may differ from the target's. The damaged target itself is refused.

**SHIP donor logs.** A SHIP log of the target's kind, with its index. For each region
one donor must cover the region's blocks *and* both anchors. Donor anchors are
matched by the entry ID from the SHIP header and position trailer alone, because a
replayed donor's first entry holds the full chain state, larger than any per-entry
read limit. Only position trailers are rebased: compressed payloads are copied
exactly.

All block sources (containers and block logs) are merged by block number. Any two
sources holding the same block must hold identical bytes, or all are refused.
Containers are capped at 1 GiB in total.

**Source directories.** `fetch-ship` and `replay-ship` write a new directory with
`source.json`, which `build --sources` reads:

```json
{
  "source": "ship_websocket | replay",
  "target_compatible": true,
  "sources": {
    "blocks": ["blocks-1234559-1234572.p2pblk"],
    "block_logs": [],
    "donors": [{"log": "trace_history-1234559-1234572.log", "index": null}]
  },
  "regions": [{"range": [1234559, 1234572], "blocks_sha256": "…", "donor_sha256": "…"}],
  "endpoints": ["ws://…"]
}
```

Paths are relative to the directory. Fetch reports also carry `trust`, and
`compatibility_error` when the data does not fit. Replay reports carry `replay`,
the nodeos command line from `replay.json`. A directory whose `target_compatible`
is not `true` is refused.

## SHIP acquisition

`fetch-ship` validates the advertised request/result ABI against the exact v0
envelope layouts it implements. For each region it requests the irreversible range
from the left anchor to the right anchor, with packed blocks, plus traces or deltas
for SHIP targets (neither for `blocks.log`).

- Up to 16 results may be in flight; each is acknowledged on receipt, and a 256 MiB
  budget bounds each endpoint's transfer per region.
- Every result must be complete and sequential. Declared IDs, numbers, previous
  positions, and irreversibility must agree with the parsed packed block header.
- With several endpoints, every block and payload byte must be identical across them.

The fetched payloads are written as per-region donor logs in Spring's stored encoding.
`build` then re-encodes them the way the target was written (below), and the fetch
checks the result against the target before it succeeds.

### SHIP payload encodings across nodeos versions

A SHIP log keeps the encoding each entry was written with; upgrading nodeos never
rewrites old entries. Every writer since EOSIO 1.8 uses boost::iostreams'
`zlib_compressor`, which calls `deflate` only with `Z_NO_FLUSH` and then `Z_FINISH`, with
window bits 15, memLevel 8, and the default strategy. Official releases link the host's
`libz.so.1`. From the release sources:

| Writer | Magic version | Preamble | Level | Fork handling |
|---|---|---|---|---|
| nodeos 1.5 to 1.7 | no `ship` magic | other header | | refused |
| EOSIO 1.8, 2.0 | 0 | u32 compressed length | default (6) | truncates the log |
| EOSIO 2.1, 2.2-rc1 | 1 | u32 compressed length; traces split prunable data | default (6) | truncates; refused |
| Leap 3.x | 0 | u32 compressed length | default (6) | truncates |
| Leap 4.0 to 5.0.x | 0 | `1`, u64 uncompressed size | default (6) | truncates |
| Spring 1.0.0-rc1 and later | 0 | `1`, u64 uncompressed size | 0 (stored) | append-only: orphaned entries |

Stored output comes from Spring's change "disable state history log file compression"
(merged 2024-04-16), and orphaned fork entries from its mutex-free, append-only log
rewrite (merged 2024-06-12). Both shipped first in v1.0.0-rc1. Chain forks keep their base's writer:
WAX's and UX's Leap builds, FIO (EOSIO 1.8 based), and Spring CE and Wire (Spring
based).

**What determines the bytes.**

- **Compressed entries:** stock zlib from 1.2.8 to 1.3.x produces identical level-6
  output for these parameters, however the input is fed. Every Ubuntu amd64 `zlib1g` from
  16.04 to 24.04 does too. zlib-ng, Chromium's, Intel's and Cloudflare's forks, and
  s390x hardware compression produce different bytes.
- **Stored entries:** the output depends on the zlib version and its buffers. zlib
  1.2.11 and later, fed by boost's 128-byte chunks, cut 32,768-byte blocks. Earlier zlib
  cut at 32,506. A payload that fills its last block exactly may be followed by an empty
  final block.

**Calibration.** The tool encodes with stock zlib compiled into it (via `libz-sys`,
never the system library). Its candidates are levels 1 to 9, and stored output at either
block size, with or without the empty final block. For each region it keeps the
candidates that reproduce both readable anchors byte for byte, under the preamble the
anchors use; anchors too large to read whole are skipped. Then:

- A source whose own entries fit the target is used as it is. Otherwise each candidate
  re-encodes the source payloads, and its result must pass the same checks: exact index
  positions, orphan gaps, and every readable byte.
- Exactly one distinct fitting result is accepted. Several distinct fitting results are
  refused, because the damaged bytes cannot tell them apart. So are anchors that agree
  on no candidate, and anchors whose preambles differ: both mean the writer changed
  inside the region.
- The chosen encoding is recorded in the evidence digest and printed per region.

Measured on real logs: an EOS mainnet full-history log is compressed (`78 9c`) up to
block 389,997,244 and stored (`78 01`) from block 389,997,245, with marker 1
throughout. Its compressed entries from genesis, and WAX's Leap 5 entries, are
reproduced exactly by level 6.

## Replay blocklog slice

`replay-ship` writes a new v3 `blocks.log` holding the snapshot head through the
last region's right anchor: `[3:u32][first:u32][chain_id:32][0xff × 8]`, then the
source entries with position trailers and `blocks.index` rebased. The chain ID is
copied from a source log that does not start at block 1. For a source that starts at
genesis, it is the SHA-256 of the packed genesis state stored in that header, which
is how Antelope derives the chain ID. nodeos rejects a chain ID that disagrees with
the snapshot, so a wrong ID cannot produce a donor.

## Review bundle

`build` writes a new directory holding `plan.json` and `replacement.bin`.
`replacement.bin` is the replacement bytes of every extent, concatenated in plan
order. `plan.json` (schema 2):

| Field | Meaning |
|---|---|
| `schema`, `tool_version` | `2`, and the version that built it |
| `log`, `index`, `kind` | Canonical target paths and log kind |
| `target` | `record_size` and `regions`: `first_block`, `last_block`, `span_start`, `span_end`, and `extents` (`offset`, `len`) |
| `log_stamp`, `index_stamp` | Size, modification time, and on Unix device and inode |
| `sources` | Canonical `blocks`, `block_logs`, and `donors` |
| `replacement_sha256` | SHA-256 of `replacement.bin` |
| `evidence_sha256` | SHA-256 over the target geometry, log header, anchor IDs, every source block and donor entry used, target positions, and every readable target byte compared |
| `source_trust` | The trust boundary statement |

The approval hash is the SHA-256 of `plan.json` itself. Apply recomputes the geometry
from the current index and the evidence from the same sources; both must match the
plan exactly.

## Apply journal

Before the first write, `apply-intent.json` is created (never overwritten) and fsynced
in the bundle. It records the plan, each extent's state, and the extents to write. If
the file already exists, apply refuses. Each unreadable extent then gets one
positional write and an fsync; a short or failed write stops apply without retrying.
After read-back and anchored verification, `receipt.json` lists the written extents.

There is deliberately no salvage-by-skipping, force overwrite, missing-index
recovery, pruning support, head truncation, or automatic fallback to a guessed
encoding. Changing a log's layout would require a separate whole-log recovery
workflow, with its own review and disk-space requirements.
