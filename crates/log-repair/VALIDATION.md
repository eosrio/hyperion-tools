# Validation

What the automated tests, the real-nodeos smoke test, the real-kernel `EIO` test, and
the real-server runs establish, and what they do not. Commands are listed in
[README.md](README.md#development).

## Rust tests

47 tests on Linux; 36 on Windows, where the scan-with-faults, media-write,
page-cache, and process-inspection tests are compiled out. Faults are injected on exact byte ranges.
A write covering a whole damaged range heals it, as a full-record write does on ZFS.

- **Formats:** a real EOS mainnet packed block whose header ID matches the ID nodeos
  reports independently. Also covered: block-log v3 and both SHIP log kinds; all
  three SHIP payload markers; corrupt zlib, sizes, magic, and pruning flags; truncated
  indexes, malformed varints, and oversized lengths. Re-encoded SHIP payloads use
  32 KiB stored blocks, checked at 0 B, one block, 49,349 B, an exact 64 KiB, and
  70,000 B.
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
  - `prepare --ship` rebuilds a stored-era trace record (block 480,000,085) as
    `already identical`, and refuses a compressed-era one (block 340,000,399) with
    `entry … length 4530 differs from target 2321`: the endpoint's payload re-encoded
    as stored zlib, against the original's compressed bytes. Nothing was written.
- `prepare` on a live, appending log fails with `target changed during fetch` once
  a fetch outlasts a block interval. Stop nodeos first.

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
- Rebuilding a compressed SHIP stretch: replay with the older release that wrote it,
  or a donor from another node with byte-identical entries for that range. Which
  release switched from compressed to stored payloads.
- Leap 3 logs (marker 0 framing is parsed by the tests only), and chain-specific
  builds other than WAX's Leap 5 build.
- `verify` without a reference does not check `blocks.log` transaction bodies: it does
  not recompute transaction Merkle roots, and signatures cover only headers.
- Valid public-certificate WSS end to end. Operators normally use `ws://` inside
  their own network.
- The snapshot head remains an operator-provided input.

Qualify a release on copies of real logs from the intended chain and node version,
ideally on a disposable filesystem with real damage beneath its checksums (as above,
or `zinject -t data` on ZFS), before relying on it for production repair.
