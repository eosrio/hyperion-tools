# Validation

What the automated tests, the real-nodeos smoke test, and the real-kernel `EIO` test
establish, and what they do not. Commands are listed in
[README.md](README.md#development).

## Rust tests

38 tests on Linux; 28 on Windows, where the scan-with-faults, media-write,
page-cache, and process-inspection tests are compiled out. Faults are injected on exact byte ranges.
A write covering a whole damaged range heals it, as a full-record write does on ZFS.

- **Formats:** a real EOS mainnet packed block whose header ID matches the ID nodeos
  reports independently. Also covered: block-log v3 and both SHIP log kinds; all
  three SHIP payload markers; corrupt zlib, sizes, magic, and pruning flags; truncated
  indexes, malformed varints, and oversized lengths.
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

## Not established

- A successful write that heals real damage: ZFS was not available, and the test
  kernel had no `dm-dust`. The healing write path is covered only by injected faults.
- ZFS itself, including `st_blksize` reporting the record size, and btrfs.
- Large historical production traces and deltas, alternate compressor versions, and
  other chain-specific node builds: the smoke chain contains empty blocks.
- Valid public-certificate WSS end to end. Operators normally use `ws://` inside
  their own network.
- The snapshot head remains an operator-provided input.

Qualify a release on copies of real logs from the intended chain and node version,
ideally on a disposable ZFS dataset with injected errors (`zinject -t data`) or a
`dm-dust` device, before relying on it for production repair.
