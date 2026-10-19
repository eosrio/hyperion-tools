# Validation

What the automated tests, the real-nodeos smoke test, the real-kernel `EIO` test, and
the real-server runs establish, and what they do not. Commands are listed in
[README.md](README.md#development).

## Rust tests

85 tests on Linux; 61 on Windows, where the scan-with-faults, media-write,
page-cache, and process-inspection tests are compiled out. Faults are injected on exact byte ranges.
A write covering a whole damaged range heals it, as a full-record write does on ZFS.

- **Formats:** a real EOS mainnet packed block whose header ID matches the ID nodeos
  reports independently. Also covered: block-log v3 and both SHIP log kinds; all
  three SHIP payload markers; corrupt zlib, sizes, magic, and pruning flags; truncated
  indexes, malformed varints, and oversized lengths. Re-encoded SHIP payloads use
  32 KiB stored blocks, checked at 0 B, one block, 49,349 B, an exact 64 KiB, and
  70,000 B.
- **Encodings (translation):**
  - *Real entries.* Seven real SHIP entries are reproduced byte for byte from their
    decompressed payloads (`testdata/ship/`): EOS mainnet compressed traces and deltas
    from blocks 50M to 300M, including a 30 KB entry; EOS Spring stored entries,
    including a three-block trace; and WAX testnet Leap 5 compressed entries. For each
    compressed entry, level 6 is the only zlib level that reproduces it.
  - *Stored layouts.* Exact fills, with and without the empty final block, and the
    older 32,506-byte split. Every codec round-trips, including an empty payload.
  - *Rebuilds.* A compressed target rebuilt from a stored donor, a stored target from a
    compressed donor, and a matching donor used as it is. A media heal of a compressed
    log from a stored donor (Unix). An upgrade inside a region (anchors in different
    encodings) and an encoding no candidate reproduces are refused, the second with the
    zlib-ng hint.
  - *Formats and replay.* Unsupported logs are named: state-history version 1 (EOSIO
    2.1/2.2), no `ship` magic (before 1.8), and pruned. Replay slices across two retained
    `blocks-A-B.log` files, and refuses a gap and a file from another chain.
- **Orphaned fork entries (SHIP):** both SHIP kinds with abandoned entries from another
  fork between an entry and the next index position. Every entry reads as in the clean
  log, and an orphan with a wrong trailer is refused. Damage beside the orphans rebuilds
  byte-exactly from a donor with or without them, and heals on media (Unix). Damage
  that reaches them is refused by the plan when the entry's header is readable, and by
  the rebuild when it is not.
- **Verify (silent corruption):** intact logs of every kind report nothing, alone, with
  a reference, and over a range. Single flipped bytes are found as follows:
  - a `blocks.log` header byte breaks the chain at the next block, including when a
    range starts after the damaged block or ends at it;
  - a `blocks.log` transaction byte is found only against a reference, at its exact
    file offset;
  - a position trailer that disagrees with the index is found;
  - SHIP payload bytes at the start, middle, and Adler-32 trailer of an entry are found;
  - a SHIP entry-ID byte is found only against a reference.

  Orphaned fork entries are counted, not reported, and a broken orphan trailer is
  found. The target as its own reference, a SHIP log as a reference, and a range
  outside the log are refused.
- **Geometry:** every pair of non-adjacent damaged records in block and trace
  fixtures, at 512 B and 1 KiB records, groups into regions whose anchors lie wholly
  outside every extent. Each such pair rebuilds byte-exactly, and some pairs share a
  region. Also covered: record-range merging, and refusal of extents in the header,
  the head entry, past the end, misaligned, unsorted, or adjacent. Unreadable bytes
  outside the known extents (for example in an anchor) make a region unrepairable.
- **Scan (Unix):** a whole-file scan finds every damaged extent: two repairable
  regions, one of them two records wide, plus an unrepairable head-entry area. The
  report and suggested `--record-index` flags match. A `--damaged-block` scan follows
  its extent outward and ignores other regions. The failed-read budget stops a scan.
  A clean scan reports nothing (on all platforms). On Linux, a page cached before a
  scan stays cached, and the pages only the scan read are dropped again. The test
  skips itself on filesystems that ignore cache eviction, such as tmpfs.
- **Reconstruction:** byte-exact two-region rebuilds for all three log kinds. Also
  covered: rebased donor trailers; a donor anchor checked from framing alone, whose
  payload is never read (on Unix it exceeds the per-entry read limit, as a full-state
  delta does; elsewhere it is small but invalid); and an intact blocks.log copy with its own
  header as the block source. Refused: block sources that disagree, the target used as
  its own source, wrong lengths, broken ID chains, gaps, duplicates, trailing
  container data, missing SHIP anchors, and readable bytes that differ while headers
  match.
- **Review and apply:** plan and replacement tampering; a missing `--node-stopped`;
  idempotent no-op apply; refusal of readable differing data; source or index changes
  after review; and per-record extent states.
- **Media writes (Unix):** two damaged regions in every log kind, each extent written
  exactly once, with read-back and anchored verification. A plan with one extent
  already identical writes only the unreadable one. Also covered: short writes, where
  the second extent is never attempted; failed read-back; existing intent journals;
  permission errors; replaced target paths; and a competing lock. On Linux, a separate
  process holding the log open blocks apply.
- **SHIP acquisition:** loopback WebSocket servers for all three target kinds and two
  regions; source directories that `build --sources` consumes directly; ABI and variant
  validation; reversible or discontinuous results; a stalled-source deadline; WSS
  crypto initialization with a non-TLS peer; two agreeing endpoints; refusal when one
  endpoint differs by a single payload byte; and duplicate endpoints.
- **Replay and CLI:** the snapshot-head limit, and a blocklog slice that reopens as a
  valid v3 log with rebased entries and the source chain ID. Value parsers and clap's
  consistency check also run.

## Copy-out tail recovery (2026-10-19)

Tracked in [issue #38](https://github.com/eosrio/hyperion-tools/issues/38).
The checks above include 19 added framing/recovery tests. They exercise byte-by-byte
cuts through the final signed block and both SHIP kinds, explicit restoration versus
salvage, missing/empty/torn/ahead/behind/incorrect indexes, readable corruption and
zero suffixes, canonical fork selection, conflicting donors, wrong chains and
anchors, source aliases, changed inputs, existing output refusal, and entries larger
than the read-ahead window. Review regressions cover source log/index/container
pathname replacement, preamble tampering with restored metadata, and donor conflicts
inside the retained prefix. Index read-ahead is checked across buffer boundaries,
backward reads and partial final slots. Whole-block framing also checks a real EOS mainnet block,
WebAuthn producer/signature fields, and packed receipts.

Windows and Linux Clippy (`--all-targets -- -D warnings`), formatting, the full scoped
Rust suites, and release builds pass. A separate Windows CLI matrix restored all
30 fixtures (including intact controls) byte for byte through the declared block;
the damaged fixture inputs remained hash-identical.

`tests/tail_smoke.py` ran against a fresh `tests/nodeos_smoke.py` development chain
with Spring `v1.2.3-alpha1-9f6e4496db1ebffec088f9206b645ef9571f2889`:

- Both choices were available for block, trace and chain-state logs with a torn last
  entry and an independently torn index. Restoration reached block 80; explicit
  salvage ended at block 79.
- `verify --against` passed for all restored pairs. A separate real nodeos process
  read copies of all three pairs and replayed through its configured block 80 limit,
  then exited successfully.
- Source logs/indexes, damaged copies, and staged recovery outputs remained unchanged.
  The completion receipts matched the staged log/index hashes.
- The existing real-nodeos smoke also passed WebSocket acquisition, local donor
  preparation, verified no-op apply, and snapshot replay for both SHIP kinds.

The final Linux tool SHA-256 for the tail smoke was
`6f88ba3ca8dfb683fc1c499bc08953c48f20762424aa78e7bd433dbb382bdef1`;
the nodeos SHA-256 was
`797315f70c789bd2d8d439f9907f86702d1c8066937c8fd12b696d271ab2b0f3`.
The run produced `result.json`, per-operation JSON/stderr, completion receipts and
`restart.log` in `/tmp/hyperion-tail-recovery-review-20261019`.

These are simulated torn files and a disposable development chain, not real
power-cut/storage-reordering qualification or production acceptance. Output
activation is manual, so interruption-safe replacement of an active log/index pair
is not claimed. Direct endpoint/replay acquisition against a broken target,
anchorless recovery, and salvage beyond an interior gap remain unimplemented.

## Real-nodeos smoke test

[`tests/nodeos_smoke.py`](tests/nodeos_smoke.py) creates a new local development
chain with 80 irreversible empty blocks, takes an early snapshot, then copies the
block and SHIP logs into an independent target directory. A second nodeos session
serves the original chain's SHIP WebSocket. Nothing is damaged here; the test checks
that every path reproduces the real on-disk bytes.

Last run 2026-10-16 with Spring `v1.2.3` on Ubuntu, in about 42 seconds:

- Whole-file scans of the real `blocks.log`, `trace_history.log`, and
  `chain_state_history.log` report no damage.
- For each log, two separate 512 B records form two repairable regions.
  `fetch-ship`, `build --sources`, and `prepare --ship` all produce replacements
  byte-identical to the targets' records, and `apply` verifies a no-op.
- `prepare --blocks-log` with the source chain's own `blocks.log` (another node's
  copy) builds an identical replacement without any network.
- `replay-ship` for each SHIP kind writes a small sliced blocklog that starts at the
  snapshot head, with the chain ID derived from the genesis header. nodeos accepts it,
  and the regenerated donors build byte-identical records.
- Every target log is unchanged at the end.

## Real-kernel EIO test

[`tests/eio_smoke.py`](tests/eio_smoke.py), run as root, copies an intact `blocks.log`
onto a small ext4 image. It maps the device sectors of one 4 KiB page of that file to
the device-mapper `error` target, so the kernel itself returns `EIO` through ext4 and
the page cache.

Last run 2026-10-16 in WSL2 (kernel 6.18), on the smoke test's `blocks.log`:

- A whole-file scan and a `--damaged-block` scan each find exactly the mapped page,
  as one repairable extent, with the 4 KiB record size taken from `st_blksize`. Reads
  of the neighboring pages still succeed despite readahead.
- `prepare --blocks-log` from the intact copy builds a plan with the extent marked
  unreadable.
- `apply` against the error target fails at the write, because the `error` target
  rejects writes too. It reports the failure, keeps `apply-intent.json`, and writes
  no `receipt.json`.
- **Observed:** after that failed write, the page cache served the replacement bytes,
  so an immediate scan reported no damage. Once caches were dropped, the damage
  reappeared. The README and apply's error message warn about this.

## Real-server runs

Run 2026-10-16 on Ubuntu 22.04 and 24.04 hosts (kernels 6.2 and 6.8, OpenZFS 2.1 and
2.2), with the static musl build. Nothing ran against a live log except read-only
`scan` and `prepare`; every write went to a copy on a disposable filesystem.

**Live nodes, read-only.** EOS mainnet (Spring 1.2.2, full history), WAX testnet
(Leap 5 WAX build), and Telos testnet (Spring 1.2.3-alpha1), all on ZFS:

- Narrow scans of `blocks.log`, `trace_history.log`, and `chain_state_history.log`
  found no damage. `st_blksize` reported the 128 KiB record size on every ZFS file.
- `prepare --ship` against the node's own state-history endpoint rebuilt
  `blocks.log` records and recent chain-state records byte for byte.
- Before the 32 KiB stored-block fix, it refused traces and older chain-state
  entries. The decompressed payloads were identical; only the stored-block split
  differed (Spring writes 32,768-byte blocks, the previous encoder 31,745). With the
  fix, the same records rebuild byte for byte. On static 3,000-block mainnet slices
  (`slice-log`), one region of 2,969 trace entries (18.5 MiB) and one of 2,877
  chain-state entries (6.4 MiB), with payloads up to 650 KB, rebuild byte for byte and
  apply as a verified no-op. The unfixed build refuses both.
- WAX's Leap 5 build writes compressed (not stored) zlib, so a SHIP fetch never
  matches its entries. The tool refuses them, as documented; donor logs work.
- **One log, several encodings.** In the EOS full-history SHIP logs (from genesis),
  sampled entries up to block 380,000,399 are compressed (`78 9c`), and from block
  420,000,006 on they are stored (`78 01`, 32 KiB blocks). The marker is 1 throughout.
  - `verify` against the node's `blocks.log` is clean on 20,000-block ranges at
    340M and at 520M, in both SHIP logs.
  - Before translation, `prepare --ship` rebuilt a stored-era trace record (block
    480,000,085) as `already identical`, and refused a compressed-era one (block
    340,000,399) with `entry … length 4530 differs from target 2321`. Nothing was
    written. Translation now rebuilds both; see below.
- `prepare` on a live, appending log fails with `target changed during fetch` once
  a fetch outlasts a block interval. Stop nodeos first.

**Translating older encodings, EOS mainnet and WAX testnet.** Run with the
translation build, which encodes with the bundled stock zlib 1.3.2.

- *Live log, read-only.* On the EOS full-history node, `prepare --ship` against its own
  endpoint now rebuilds the compressed-era trace and chain-state records at block
  340,000,399 as `already identical`, `re-encoded as zlib level 6`.
- *Static slices.* These are copies of the EOS SHIP logs, with trailers and index
  rebased, tested one 4 KiB record at a time with `prepare --ship`:

  | Slice | Records | Result |
  |---|---|---|
  | blocks 340,000,000 to 340,020,000, both logs | 40 | all `already identical`, re-encoded as zlib level 6 |
  | blocks 120,000,000 to 120,010,000, both logs (a 842 MB trace slice, about 84 KB per entry) | 40 | all `already identical`, re-encoded as zlib level 6 |
  | around the switch at block 389,997,245, both logs | 14 | before it, re-encoded as zlib level 6; after it, the endpoint's stored bytes as they are; the record holding both encodings refused (`different encodings`) |

- *Real damage, healed.* On a disposable btrfs, four records were damaged beneath the
  data checksums in each of six files (18 extents):
  - EOS slices (340M) from the EOS endpoint over the LAN;
  - WAX testnet slices (written on a zlib 1.3 host) from the WAX endpoint;
  - the same EOS slices from a donor in the other encoding (the payloads rewritten as
    Spring's stored zlib), with the fetched blocks.

  Every region was rebuilt `re-encoded as zlib level 6`. After a remount, all files
  scanned clean and were byte-identical to the originals.
- *Replay.* The real-nodeos smoke test (Spring 1.2.2), including sliced snapshot
  replay, passes with the translation build.

**Orphaned fork entries, Libre mainnet (Spring 1.2.3-alpha1, ext4).** A static
30,000-block copy of each SHIP log keeps all 1,859 orphaned fork entries; positions and
trailers are rebased. It was tested with `prepare --ship` against the node's own endpoint,
one 4 KiB record at a time:

| Records sampled | Before the orphan fix | After |
|---|---|---|
| 30 with no orphans nearby | `already identical` | `already identical` |
| 21 next to orphans, which lie outside the record | refused (`does not fit the target`) | `already identical` |
| 30 holding orphan bytes | refused, blaming the encoding | refused, naming the orphans |

The same copies on a disposable btrfs had 8 records damaged beneath the data
checksums, 6 of them next to orphans. Whole-file scans found all 8 as repairable,
`prepare --ship` and `apply` wrote them, and after a remount both files were
byte-identical to the originals.

**Copies of real WAX testnet logs.** These are `blocks.log` (1.3 GB), `trace_history.log`
(1.8 GB), and `chain_state_history.log` (4.9 GB), with real transactions. The index was
copied before the log, then both were trimmed to the last complete entry.

- **No damage:** `prepare --ship` (another node's endpoint) and `prepare --blocks-log`
  (another node's `blocks.log`, written by a different node version) each found
  `already identical`, and `apply` verified a no-op with the file hash unchanged.
  Apply refused a wrong `--approve` hash, a missing `--node-stopped`, and another
  process holding the log open.
- **Real damage, healed.** Each damage was written beneath the filesystem's own
  checksums, so the kernel returned real `EIO`. Every case had three regions in
  `blocks.log` and two in each SHIP log, including an extent of two adjacent records.

  | Filesystem | Damage | Records |
  |---|---|---|
  | ZFS, `compression=off` and `lz4`, file-backed pool | bytes overwritten inside the L0 blocks on the vdev | 128 KiB, 10 records |
  | btrfs, single data profile | bytes inverted in the data extents | 4 KiB, 10 records |
  | ext4 and xfs on dm-integrity (crc32c, separate metadata device) | bytes inverted on the data device | 4 KiB, 10 records each |

  Whole-file scans found exactly the damaged records, and every region was
  repairable. `prepare` used `--ship` or `--blocks-log` for blocks, and `--donor-log`
  with `--blocks-log` for SHIP logs. After `apply`, the caches were dropped: the ZFS
  pool was exported and imported, and the other filesystems were unmounted, with the
  dm-integrity devices closed and reopened. Then every repaired record read with
  `O_DIRECT`, whole-file scans were clean, and all files were byte-identical to their
  pre-damage hashes. On ZFS the records had new block pointers and `zpool scrub`
  reported no errors.
- **Observed on XFS:** after a failed read, the scan's retries of neighboring records
  in the same large folio fail too, so one bad sector was reported as up to 64 KiB.
  Apply rewrote those bytes with identical data. Documented in the README.
- **Observed on btrfs:** the repaired files read cleanly, but `btrfs scrub` still
  counted the old sectors, because the partly used old extents stay allocated.
  `btrfs filesystem defragment` did not release them; rewriting the files did, and
  scrub then reported no errors. Documented in the README.
- **Observed on ZFS:** the plan pins the file's device number, which an export and
  import changes, so apply refuses an older plan. The refusal now names the field.

**`verify` on live nodes, read-only.** Nothing was found anywhere:

| Logs | Filesystem | Coverage | Result |
|---|---|---|---|
| Six testnet `blocks.log` files: Telos, WAX, Ultra, Jungle, Proton, and a Telos test client | ext4 | whole files, 46,919,455 entries; 6.1 GiB in 23 s for the largest | no problems |
| Libre mainnet `blocks.log`, against another node's `blocks.log` | ext4 | 5,000,001 entries, 966 MiB in 2.2 s | no problems |
| Libre mainnet trace and chain-state logs, against the node's `blocks.log` | ext4 | 5,000,001 entries each, 2.2 and 4.0 GiB in about 50 s | no problems; 449,043 orphaned fork entries each |
| EOS mainnet trace and chain-state logs, against the node's `blocks.log` | ZFS | 500,001 entries each, 3.3 and 1.5 GiB in 25 and 20 s | no problems; 44 orphaned fork entries each |

An early `verify` build assumed that every SHIP entry fills its index span. It reported
every orphaned fork entry on Libre as corruption, and that is how the orphan layout was
found.

## Not established

- A disk that physically remaps sectors on write. ZFS, btrfs, and dm-integrity heal
  the same way, but `dm-dust` was not available to emulate that directly.
- Replay (`replay-ship`, `prepare --nodeos`) on a real chain; it ran only on the
  smoke chain.
- Replay of a compressed stretch, with an older release or with Spring: whether the
  replayed payloads equal the ones the original release wrote. The readable bytes
  check refuses any difference, but it has not been run on a real chain.
- Logs written on a host with non-stock zlib (zlib-ng, s390x hardware compression):
  refused by calibration, not tested on a real log.
- Leap 3 and EOSIO 1.8/2.0 logs written by those releases themselves (their legacy
  u32 preamble is covered by unit tests), and chain-specific builds other than WAX's
  Leap 5 build. The EOS log tested here was written with the Leap 4+ preamble
  throughout.
- An upgrade inside a damaged region (refused, not translated).
- `verify` without a reference does not check `blocks.log` transaction bodies: it does
  not recompute transaction Merkle roots, and signatures cover only headers.
- Valid public-certificate WSS end to end. Operators normally use `ws://` inside
  their own network.
- The snapshot head remains an operator-provided input.

Qualify a release on copies of real logs from the intended chain and node version,
ideally on a disposable filesystem with real damage beneath its checksums (as above,
or `zinject -t data` on ZFS), before relying on it for production repair.

## Real-log inspection performance (2026-10-19)

[PERFORMANCE.md](PERFORMANCE.md) records full inspection of a frozen 56.4 GB Libre
block log containing 269,898,278 blocks. Buffered index reads preserve the entire
inspection result, including both input hashes and every index discrepancy check.
The Linux benchmark recorder captures artifact identity, input stamps, elapsed and
CPU time, peak RSS, process read counters and the operator's cache description.
These measurements cover read-only `recover-tail`, not media scanning or copy-out
restoration.

## In-place tail trim qualification

Linux unit tests cover exact saved suffixes, retained-prefix preservation,
interruption after journal commit, either truncate/fsync and final read-back;
resume or rollback from every durable boundary; rollback interruptions and partial
appends; idempotent completion; wrong/missing/aliased indexes; competing locks;
missing stop acknowledgement; changed prefixes, paths, intent and saved bytes;
appends after preparation/read-back; and same-length writes followed by mtime
restoration to both journal artifacts and targets. The target guards include Linux
ctime. An intact log with excess whole/partial index slots is also trimmed and
rolled back exactly.

`tests/libre_trim_smoke.py` is an optional destructive test for the disposable,
frozen Libre benchmark copy described in PERFORMANCE.md. It requires an explicit
`--discard-copy-history`, refuses copies outside `/tmp` and aliases/hard links to
the live source, and verifies the exact reported incident hashes before invoking
the tool. It tests trim, a manually constructed log-trimmed/index-original resume
state, and byte-exact rollback; it retains journal and per-stage evidence. This is
not an actual power-cut or storage-flush fault qualification.

On 2026-10-19, the real Libre benchmark copy was reduced to exactly the incident's
48,477,356,032 log bytes and 1,853,814,976 index bytes. Independent SHA-256 matched
the operator's damaged files before testing. The live node stayed running and its
historical samples and file identities remained unchanged; only the disposable
copy was modified.

| Operation | Elapsed | User CPU | System CPU |
|---|---:|---:|---:|
| Inspect, save suffixes, trim, full parser read-back | 131.75 s | 104.27 s | 27.28 s |
| Resume simulated log-trimmed/index-original state | 96.25 s | 71.08 s | 25.05 s |
| Rollback and verify damaged originals | 61.48 s | 37.55 s | 23.83 s |

The retained pair contained blocks 1..231726802, with 327 saved log bytes and 560
saved index bytes. Both target inodes were preserved. Independent hashes checked
the trimmed output, the resumed output and the rolled-back originals. Timing
excludes fixture preparation and the independent Python hash oracles. Normal page
cache conditions were used, without a global cache flush. This qualifies the
software recovery sequence on this corpus; node startup/replay and storage power
loss behavior remain separate acceptance checks.

- Damaged/restored-original log SHA-256:
  `b5e68e7aaac92ab15863a00dab57a2afdc4ad0367262cd6b35ba8155691c094a`.
- Damaged/restored-original index SHA-256:
  `af4a7a66594d5c49039589dc03590d92b3f6fe94b0551291f5f16c99ebd1e9a7`.
- Trimmed log SHA-256:
  `04f7f212f2cd5bf3b28ed3ee4c9d2a08131687ebd7512d8ed3989f19b1622b1a`.
- Trimmed index SHA-256:
  `257f719c34b6f583991971d2e6d3219addf1d42194f40e7e6dca98c3c2c51e87`.
- Qualification binary SHA-256:
  `7b63c983752de9f986ce47fcef58ca234802e701fbe7c54c424e9abad99ea3d8`.

The final operator-help/summary wording was updated afterward; the qualified trim,
resume and rollback implementation is unchanged. The harness's safety checks also
reject an aliased fixture under optimized Python (`-O`).
