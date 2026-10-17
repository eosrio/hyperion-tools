# antelope-log-repair

Finds and repairs **unreadable records** inside Antelope `blocks.log`,
`trace_history.log`, and `chain_state_history.log` files, including retained range
files, without resyncing the node.

When a disk or filesystem returns an I/O error for part of a log, nodeos can no
longer serve those blocks. This tool locates the unreadable bytes and rebuilds the
affected entries from independent sources. It proves that they fit the surrounding
readable data, and it writes back only the unreadable bytes, after you approve a
reviewed plan.

Part of the [Hyperion Tools](../../README.md) Cargo workspace, MIT licensed
([LICENSE](../../LICENSE)). Status: **experimental**. Qualify it on copies of your own
logs before relying on it in production (see [Validation](#validation)).

**Contents:**
[Quick start](#quick-start) ·
[What it can repair](#what-it-can-repair) ·
[How it works](#how-it-works) ·
[1. Find the damage](#1-find-the-damage) ·
[2. Get recovery data](#2-get-recovery-data) ·
[3. Build and review](#3-build-and-review) ·
[4. Apply](#4-apply) ·
[5. After the repair](#5-after-the-repair) ·
[Multiple damaged records](#multiple-damaged-records) ·
[Check for silent corruption](#check-for-silent-corruption) ·
[Troubleshooting](#troubleshooting) ·
[Command reference](#command-reference) ·
[Limits](#limits) ·
[Development](#development)

## Install

From the Hyperion Tools repository root:

```sh
cargo build --release --locked --package antelope-log-repair
./target/release/antelope-log-repair --help
# or
cargo install --path crates/log-repair --locked
```

Tagged releases also ship a static Linux binary in the toolkit tarballs.

## Quick start

nodeos fails to read block 1,234,567 from `blocks.log`, and the kernel log or
`zpool status` shows I/O errors on that file.

**1. Find the damage.** Read-only; nodeos may keep running.

```sh
antelope-log-repair scan --log /data/blocks/blocks.log --damaged-block 1234567
```

```text
Scanned /data/blocks/blocks.log (blocks.log): 128 KiB in 0.0s, record size 128 KiB, 2 failed read(s).
Found 1 damaged area(s), 128 KiB in 1 extent(s):
  [1] blocks 1234560..1234571: 128 KiB in 1 extent(s), record(s) 7911: repairable

Next (nothing is written, but stop nodeos first: the plan pins the log's size and modification time, which a running node changes with every block):
  antelope-log-repair prepare --log /data/blocks/blocks.log --record-size 131072 --record-index 7911 --ship ws://HOST:PORT --out DIR
Instead of --ship, --blocks-log PATH uses an intact blocks.log from another node or a backup.
```

**2. Stop nodeos, then prepare a reviewed plan.** This acquires recovery data and builds
a bundle. It is still read-only on the target. Stop nodeos first: the plan pins the log's
size and modification time, and a running node appends every block, so a fetch fails
with `target changed during fetch` and any plan goes stale before apply.

```sh
systemctl stop nodeos          # or however you run it
antelope-log-repair prepare --log /data/blocks/blocks.log --record-size 131072 \
  --record-index 7911 --ship ws://ship-a.example:8080 --ship ws://ship-b.example:8080 \
  --out /scratch/repair-1
```

```text
Built repair bundle /scratch/repair-1/bundle
  log:      /data/blocks/blocks.log (blocks.log)
  damage:   1 region(s), 1 extent(s), 128 KiB (record size 128 KiB)
  [1] blocks 1234560..1234571: 128 KiB in 1 extent(s), record(s) 7911
  state:    1 extent(s) unreadable (will be written), 0 already identical
  ...
  plan_sha256: da133837…

Review plan.json and the source provenance, stop nodeos, then:
  antelope-log-repair apply --bundle /scratch/repair-1/bundle --approve da133837… --node-stopped
```

**3. Apply the approved plan,** with nodeos still stopped.

```sh
antelope-log-repair apply --bundle /scratch/repair-1/bundle --approve da133837… --node-stopped
```

Then verify the media (for ZFS: `zpool scrub`, then `zpool clear`), restart nodeos,
and check the repaired blocks through its API. The outputs above are abbreviated, with
illustrative numbers. Every command also accepts `--json` for automation.

## What it can repair

The tool repairs bytes the operating system **refuses to read**: reads that fail with
`EIO` or `ENODATA`. Only those errors authorize a write.

| Situation | Supported? |
|---|---|
| ZFS permanent data error (checksum/IO) on a log file | Yes: the design target. Healed real on-disk damage (see [Validation](#validation)) |
| btrfs checksum error on a log file | Yes, with 4 KiB records. Healed real checksum damage; see [After the repair](#5-after-the-repair) for scrub |
| ext4/xfs on a disk with unreadable sectors | Yes, with 4 KiB records, the filesystem default. Healed real sector errors on dm-integrity (see [Validation](#validation)) |
| Silent corruption: the bytes read back, but wrong | **No.** It never overwrites readable data. [`verify`](#check-for-silent-corruption) detects it |
| Damage in the first entry, the head (last) entry, or the file header | No: a readable neighbor entry is needed on both sides |
| Damaged or missing `.index` | No: the index defines entry boundaries |
| SHIP damage inside orphaned fork entries (abandoned entries nodeos leaves after a fork) | No: no other node, endpoint, or replay holds those bytes. Damage next to them is repaired, and they are kept as they are (see [FORMAT.md](FORMAT.md#target-logs)) |
| Pruned block logs, SHIP logs with feature flags | No: refused as unsupported formats |
| A truncated log, or damage at the end of the file | No: use your node's normal recovery for the head of the log |

It runs on Linux and other Unix systems. On Windows, `scan`, `prepare`, `build`, and
the fetch commands work for review on copied files, but writing is disabled.

## How it works

```text
            readable    damaged extent     readable
  ... ──┬───────────┬────────────────────┬───────────┬── ...
        │  block 20 │ 21 │ 22 │ ... │ 43 │  block 44 │
        └─ anchor ──┴───── region ───────┴─ anchor ──┘
```

- **Record:** the filesystem's unit of reading and writing. On ZFS it is the file's
  record size, usually 128 KiB. On ext4/xfs/btrfs it is 4 KiB. When one byte of a
  record is bad, the whole record fails to read.
- **Extent:** a contiguous run of unreadable records.
- **Region:** the log entries that overlap one or more extents, plus a readable
  **anchor** entry on each side. Block IDs chain from the left anchor to the right
  one, so the anchors tie the rebuilt entries to *your* chain and fork.

To repair a region the tool:

1. takes the packed blocks for the region from your sources, and checks that their ID
   chain runs from the left anchor to the right one;
2. for SHIP logs, takes the trace or delta entries for those blocks from a donor log,
   checking each entry's block ID;
3. lays the entries out at the target's own offsets, using the target's intact index,
   and rewrites each entry's position trailer;
4. requires every readable byte of the affected entries to match the target exactly,
   and every entry length to match the index;
5. keeps only the bytes of the unreadable extents as the replacement.

The plan pins all of this with hashes: the target's geometry, file sizes and times, the
anchors, every source byte consulted, the readable bytes compared, and the
replacement. You approve the plan by its hash. `apply` repeats every check, then writes
each unreadable extent with a single positional write, and nothing else.

## 1. Find the damage

`scan` reads the log and reports every unreadable extent, the blocks each one affects,
and whether it can be repaired. It never writes, and nodeos may keep running.

Choose how much to read:

| Flag | Reads | Use when |
|---|---|---|
| *(none)* | The whole file | You know the file but not where. Costly on a live node: see below |
| `--damaged-block N` | The records holding block N's entry, following the damage outward | nodeos or a client reported a failing block number |
| `--block-range A-B` | The records holding blocks A through B, following the damage outward | You know an approximate range |
| `--record-index N` or `A-B` (repeatable) | Nothing; you name the records | You already have them, for example from ZFS (below) |

**Cost of a whole-file scan.** It reads every byte at sequential disk speed. On a
mainnet node, `blocks.log` and the SHIP logs can be several terabytes, which means hours
of heavy reads competing with nodeos. Start with `--damaged-block` or `--block-range`,
and run full scans on a copy, a quiet node, or off-peak. The scan prints its progress,
rate, and time left every 4 GiB, and Ctrl-C stops it safely. On Linux it drops the pages
it alone brought into the page cache, so cold log data does not evict a live node's
working set. Pages already cached stay cached.

**Record size.** By default it is the file's filesystem block size (`st_blksize`). On
ZFS that is the file's own record size; on ext4/xfs/btrfs it is 4 KiB. Override it with
`--record-size BYTES` (decimal or `0x` hex) if the filesystem reports something else.
For example, a ZFS dataset's `recordsize` changed after the file was created does not
change that file's existing block size.

**XFS.** XFS caches file data in large folios (up to 64 KiB here). After a failed read,
the scan's retries of neighboring records in the same folio fail too, so one bad sector
can be reported as an extent of up to 64 KiB. This is safe: apply writes only bytes
rebuilt from the sources and checked against the anchors, which match the healthy
sectors. Your sources must cover the wider region.

**From a ZFS error report.** `zpool status -v` lists files with permanent errors.
`zpool events -v` shows the details in the checksum or io ereport:

- `zio_object` is the file's inode number (`ls -i FILE`);
- `zio_level` must be `0` (a data block; indirect-block damage is out of scope);
- `zio_blkid` is the record index: pass it as `--record-index` (alias `--zfs-blkid`);
- `zdb -dddd POOL/DATASET OBJECT` shows the file's record size in the `dblk` column.

**Failing devices.** A scan stops after 1,024 failed reads (`--max-bad-reads`). Damage
that widespread is no longer a record repair: check the device and the pool first.
On a physically failing disk every failed read can take seconds, so narrow the scan
with `--damaged-block` or `--block-range`.

**Reading the report.** Each damaged area is marked `repairable` or `NOT repairable`
with a reason (see [Troubleshooting](#troubleshooting)). `scan --json` also gives the
anchor block IDs, each region's `source_block_range` (the blocks a source must
supply), and, for SHIP logs, `snapshot_latest_head_block` for replay.

## 2. Get recovery data

All acquisition is read-only on the target, and writes only into a **new** directory.
Every source is checked again at build and at apply. Choose one:

| Source | Log kinds | Command |
|---|---|---|
| Other nodes' state-history (SHIP) endpoints | all | `prepare --ship URL` or `fetch-ship` |
| An intact `blocks.log` from another node or a backup | `blocks.log` (and block sources for SHIP) | `prepare --blocks-log PATH` |
| Replaying an intact blocklog from a snapshot | SHIP logs | `prepare --nodeos … --snapshot …` or `replay-ship` |
| Files you already have | all | `--blocks`, `--blocks-log`, `--donor-log`, `--sources` |

### State-history endpoints

```sh
antelope-log-repair fetch-ship --log /data/state-history/trace_history.log \
  --damaged-block 1234567 \
  --ship ws://ship-a.example:8080 --ship ws://ship-b.example:8080 \
  --out /scratch/ship-source
```

This fetches each region's blocks, plus traces or deltas for SHIP targets, from every
endpoint. The range is irreversible only, and every endpoint must return
byte-identical data, or all sources are refused. Pass one `--ship` per **independent
operator**. `ws://` and `wss://` both work; TLS uses public root certificates, and
URLs with embedded credentials are refused. `--timeout-secs` (default 120, max 3600)
applies per endpoint and region.

The output directory holds one `blocks-FIRST-LAST.p2pblk` per region and, for SHIP
targets, a donor `trace_history-FIRST-LAST.log`/`.index` (or `chain_state_history`).
It also holds `source.json` with the endpoints, digests, and compatibility verdict.
Pass the directory to `build --sources`.

SHIP endpoints send uncompressed history payloads, not disk entries. The tool
re-encodes them in the stored-zlib framing found on the target's anchors, split into
32 KiB stored blocks as Spring writes them (measured on Spring 1.2.2 and 1.2.3 logs). If the original node used another
compressor, debug setting, or node version, the lengths or readable bytes will not
match. Some chain-specific builds write compressed (not stored) zlib, for example WAX's
Leap 5 build; their entries never match a SHIP fetch. The fetch then fails and keeps its files and report
for diagnosis, and build refuses them. Use replay, or a donor log from a node with
the same configuration.

### Another node's blocks.log

```sh
antelope-log-repair prepare --log /data/blocks/blocks.log --damaged-block 1234567 \
  --blocks-log /mnt/other-node/blocks/blocks.log --out /scratch/repair-1
```

Any intact `blocks.log` covering the regions works: another of your nodes, a peer's
copy, or a backup. Its `blocks.index` must sit alongside it. Header offsets may differ;
only the blocks are used. Repeat `--blocks-log` to use several. Where they overlap,
they must agree byte for byte. The damaged target itself is refused as a source.

### Replay from a snapshot (SHIP logs)

Packed blocks contain no traces or deltas. If no endpoint holds the history, regenerate
it with the same nodeos version that wrote the original:

```sh
antelope-log-repair prepare --log /data/state-history/chain_state_history.log \
  --damaged-block 1234567 \
  --nodeos /opt/spring/bin/nodeos \
  --snapshot /snapshots/snapshot-1230000.bin --snapshot-block 1230000 \
  --blocks-dir /data/blocks --chain-state-db-size-mb 32768 \
  --out /scratch/repair-1
```

- The snapshot must be an uncompressed portable snapshot (format 1). It must be taken
  **at most two blocks before the first damaged block**: the first replayed block
  emits a full-state delta, which must precede the damage. `scan` prints the latest
  allowed head.
- `--snapshot-block` is the head you verified when obtaining the snapshot. The tool
  does not read it from the snapshot; nodeos validates the snapshot contents.
- `--blocks-dir` holds an intact `blocks.log` + `blocks.index` covering the snapshot
  head through the last damaged region. The tool copies **only that range** into a
  fresh, valid v3 blocklog in the sandbox, so space and time grow with the distance
  from the snapshot to the damage. Pick a recent snapshot.
- nodeos runs only inside the new sandbox: an empty config, P2P and HTTP disabled,
  no producer, a loopback SHIP endpoint, and `--terminate-at-block` after the last
  region. `replay.json` records the exact command; nodeos's output is saved alongside.
- Match the original node: `--trace-history-debug-mode` if it was set, and a large
  enough `--chain-state-db-size-mb` for the snapshot (default 1024).

One replay covers every damaged region in the log.

### Files you already have

- `--blocks FILE`: a `P2PBLK01` block container (format in [FORMAT.md](FORMAT.md)).
- `--blocks-log FILE`: an intact `blocks.log`.
- `--donor-log FILE`: a SHIP log of the target's kind covering a region's blocks,
  including both anchors; its `.index` must sit alongside. Its file offsets may differ:
  only the entries are used.
- `--sources DIR`: a `fetch-ship` or `replay-ship` output directory.

All are repeatable and can be combined.

## 3. Build and review

`prepare` locates, acquires, and builds in one step. You can also run the steps
yourself:

```sh
antelope-log-repair build --log /data/state-history/trace_history.log \
  --record-index 7911 --sources /scratch/ship-source --out /scratch/repair-1/bundle
```

The bundle is a new directory holding `plan.json` and `replacement.bin`; existing paths
are never reused. Build refuses if any damaged record is readable but different from
the rebuilt bytes.

**Before approving, review:**

- the target `log` path and `kind` in `plan.json`;
- `target.regions`: the blocks and extents to be written;
- `sources`: every file used, and where it came from (the `source.json` of fetched or
  replayed sources lists the endpoints or the replay command);
- the `plan_sha256` printed by build: it is the hash of `plan.json` itself.

**Trust boundary.** Block IDs authenticate block *headers*. The tool does not check
transaction signatures or recompute transaction and action Merkle roots, and SHIP
traces and deltas are not covered by block IDs at all. Agreement between independent
operators, or your own replay, is what makes the content trustworthy. The plan's hashes
make later tampering detectable, but they cannot make an untrusted source authentic.

## 4. Apply

Stop nodeos and anything else reading or writing the log or its index, then:

```sh
antelope-log-repair apply --bundle /scratch/repair-1/bundle --approve PLAN_SHA256 --node-stopped
```

Apply refuses unless:

- `--approve` matches the plan's hash, and `replacement.bin` matches the plan;
- no other process has the log or index open (checked through `/proc` on Linux; run
  as root so other users' processes are visible). Advisory locks also exclude
  concurrent repair runs, but nodeos does not honor them;
- the file identity, size, modification time, and index are unchanged since build. The
  identity includes the device number, which a ZFS pool re-import or a reboot changes:
  build a fresh plan after either;
- every damaged extent is either unreadable, or readable and already identical. A
  readable, *different* record is never overwritten;
- rebuilding from the same sources gives exactly the approved bytes and evidence.

Then it writes `apply-intent.json` durably into the bundle, and **writes each
unreadable extent once**, with a single positional write and an fsync. Extents that
already match are skipped. It never truncates the log or touches the index, and there
is no `--force`.

After writing, it reads every extent back, parses the repaired entries (including
zlib streams for SHIP), rechecks the anchored ID chain, and writes `receipt.json`. The
read-back may be served from the filesystem cache, so it proves what was written, not
what the media holds. That is what the scrub in step 5 is for.

**If apply fails part-way.** An unreadable record cannot be backed up. The bundle holds
the verified replacement and evidence, but no rollback copy. Apply never retries a
failed or short write, and the intent journal blocks a second attempt with the same
bundle.

- Keep the bundle, and check the device or pool first: a write that fails usually
  means the storage is still failing.
- **Do not trust a scan made right after a failed write.** The page cache can keep
  serving the replacement bytes, so the file looks repaired while the device still
  fails. This was observed with real kernel errors. Drop caches
  (`echo 3 > /proc/sys/vm/drop_caches`) or remount, then scan again.
- Then build a fresh plan. A write that did complete but lost its receipt shows up as
  an "already identical" extent, and a new plan's apply verifies it as a no-op.

## 5. After the repair

1. Verify the media with the filesystem's own tools. For ZFS, run `zpool scrub POOL`;
   once it reports no errors, run `zpool clear POOL` so the old error records are
   dropped. On a disk with bad sectors, check SMART reallocation counters.
   - **btrfs:** the repaired file reads cleanly at once, but `btrfs scrub` keeps
     reporting the old sectors. Copy-on-write placed the new bytes elsewhere, and the
     old extent stays allocated while the rest of it is still in use. Its log messages
     even name the repaired file. `btrfs filesystem defragment` does not release it.
     Rewriting the file does: with nodeos stopped, `cp --reflink=never LOG LOG.new &&
     mv LOG.new LOG`, which needs free space for one extra copy.
2. Restart nodeos.
3. Fetch the repaired blocks (and traces or deltas) through its HTTP API or SHIP, and
   compare them with another node.

## Multiple damaged records

The tool handles any number of damaged records in one plan, up to the
[limits](#limits):

- **Extents:** adjacent unreadable records merge into one extent. Each extent is written
  with one write, which always covers whole filesystem records, even if the real
  record size is larger than the one detected.
- **Regions:** extents whose affected entries overlap, or sit directly next to each
  other, share one region. There must be no readable entry between them that could
  serve as an anchor. Separate regions each get their own pair of anchors, and may
  share one anchor entry.
- **One plan, one approval:** a plan covers every region in the log. Sources must
  cover every region's `source_block_range`: `fetch-ship` and `replay-ship` take care
  of that, and a `blocks.log` copy usually covers it.
- **Partial states:** an extent that is already readable and identical, for example
  after an interrupted earlier repair, is verified and skipped. Only unreadable
  extents are written.
- **Unrepairable areas:** `scan` reports each area separately. `prepare`, `build`, and
  the fetch commands refuse the whole selection if any area in it is unrepairable.
  Narrow the selection (`--record-index`, `--block-range`) to repair the others.

If a region's anchors or readable gaps turn out to be unreadable too, the refusal says
so: scan a wider range (or the whole file), so the neighboring damage is included in
the plan.

## Check for silent corruption

`scan` finds bytes the system **refuses** to read. On ZFS and btrfs that covers rotten
data too, because their checksums turn it into a read error. On ext4 and xfs, which
have no data checksums, rotten bytes read back without any error, and `scan` cannot
see them. `verify` checks what the logs prove about themselves. It is read-only and
can run while nodeos is running: it checks only entries that exist when it starts.

```sh
antelope-log-repair verify --log /data/blocks/blocks.log --block-range 1200000-1300000
antelope-log-repair verify --log /data/blocks/blocks.log --against /mnt/other-node/blocks/blocks.log
antelope-log-repair verify --log /data/state-history/trace_history.log --against /data/blocks/blocks.log
```

| Log | Checked from the file alone | Added by `--against BLOCKS_LOG` |
|---|---|---|
| `blocks.log` | each entry's position trailer; header block numbers; the ID chain, so a changed header byte breaks the link to the next block | every packed block, byte for byte, with the first differing file offset |
| SHIP logs | trailers and block numbers; every payload's zlib framing, sizes, and Adler-32 checksum, so any changed payload byte fails | each entry's block ID |

- **Not covered without a reference:** `blocks.log` transaction bodies and signatures,
  because nothing in the file checks them. Pass `--against` with another node's
  `blocks.log` (its `blocks.index` alongside). For a SHIP log, the node's own
  `blocks.log` is a useful reference for the entry IDs. The report lists what was and
  was not checked.
- **Orphaned fork entries** in SHIP logs are counted and their framing checked; they are
  not problems (see [FORMAT.md](FORMAT.md#target-logs)).
- **Cost:** without `--block-range` it reads the whole file, as a whole-file scan does,
  and drops the pages only it read from the cache. Expect up to about 1 GB/s on
  `blocks.log`, and less for SHIP logs with `--against`.
- **Output:** each problem names the block, its byte range, and the reason. `--json`
  sets `corrupt` and lists `findings`. The exit status is 0 when the check completes,
  as with `scan`. It stops after `--max-findings` (default 1000).

`verify` writes nothing and does not repair. The bytes it flags read back without an
error, so `apply` will not overwrite them. Restore those entries from another node or a
backup, or resync. Run `scan` too: an I/O error during `verify` is reported as
`unreadable`.

## Troubleshooting

| Message | Meaning | What to do |
|---|---|---|
| `no unreadable data found in the selected range` | Every byte read back | Check the path. Scan a wider range or the whole file. On ext4 or ZFS, damage seen earlier may have been rewritten or remapped since |
| `touches the log's first or head entry` | No readable entry on one side | Out of scope. For the head, use your node's normal recovery for the end of the log |
| `overlaps the log header` / `reaches past the last entry` | Damage in the file header or tail | Out of scope |
| `byte N near this damage is unreadable too` | An anchor or readable gap also fails | Scan a wider range or the whole file so all nearby damage is in one plan |
| `stopped after N failed reads` | Widespread damage | Check the device and pool health. Raise `--max-bad-reads` only if you are sure |
| `this platform reports no filesystem block size` | Windows, or an unusual filesystem | Pass `--record-size` |
| `followed by orphaned fork entries … reaches them` | The damage covers entries nodeos abandoned after a fork, which no source holds | Not repairable. Narrow the selection to the other regions; for these bytes, resync or restore the log |
| `entry N length … differs from target` | The source encodes this entry differently: another compressor, node version, or debug setting | Use replay with the original nodeos version and settings, or a donor from an identical node |
| `rebuilt bytes … differ from the readable target bytes` | The source does not match your log | Wrong chain or fork, or a differently configured source node |
| `belongs to another chain or fork` | Anchor IDs do not match | The source is from another chain or fork |
| `SHIP endpoints disagree` / `block sources disagree` | Two sources gave different bytes | Find out which one is wrong. Use only sources you trust |
| `block sources lack block N` / `no donor log covers blocks A..B` | A source does not cover a region | Fetch or provide that range, including both anchors |
| `snapshot head must be at or before N` | The snapshot is too late | Use an older snapshot |
| `process(es) [...] have … open` | Something still has the file open | Stop nodeos and any other reader or writer |
| `target changed during fetch` | nodeos is still running and appended blocks | Stop nodeos, then run the fetch or prepare again |
| `log changed since build` / `index changed since build` | The files changed after review; the message names the fields | Build and review a new plan. `device` alone means a pool re-import or reboot, not a content change |
| `readable and differ from the approved replacement` | A damaged record became readable with different bytes | Investigate: the tool never overwrites readable data |
| `write bytes … ; inspect apply-intent.json` | A write failed or was short | See [If apply fails part-way](#4-apply) |
| `plan hash does not match --approve` | Wrong hash, or `plan.json` was edited | Approve the hash printed by build, after reviewing |

## Command reference

| Command | Purpose | Writes |
|---|---|---|
| `scan` (alias `inspect`) | Find and classify damage | Nothing |
| `verify` | Find entries that read back but are wrong | Nothing |
| `prepare` | Locate, acquire (`--ship`, replay options, or local files), and build | A new `--out` directory |
| `fetch-ship` | Acquire from SHIP endpoints | A new `--out` directory |
| `replay-ship` | Regenerate SHIP history in a sandbox | A new `--out` directory |
| `build` | Verify sources and write a review bundle | A new `--out` directory |
| `apply` | Write the approved replacement | The target's unreadable extents, plus the journal and receipt in the bundle |

Selection flags shared by scan, prepare, fetch-ship, replay-ship, and build: `--log`,
`--index` (default: alongside), `--record-size`, `--record-index`/`--zfs-blkid`,
`--damaged-block`, `--block-range`, `--max-bad-reads`. Run any command with `--help`
for every option.

Output is a human summary on stdout. With `--json`, it is one JSON document on stdout,
also for errors (`{"ok": false, "error": …}`). Progress and warnings go to stderr. The
exit status is 0 on success, including a scan that finds nothing, and 1 on any error
or refusal.

## Limits

- Formats: non-pruned block-log v3, and non-pruned SHIP v0, including legacy payload
  size markers and Leap 4+ uncompressed-size headers. Unknown features fail closed.
- Record size: a power of two from 512 B to 16 MiB.
- Per plan: up to 64 regions and 256 MiB of damaged bytes.
- Per region: up to 9,998 affected blocks and 256 MiB of affected entries.
- 256 MiB caps on a single entry, a per-endpoint SHIP transfer, and a decompressed
  payload; block sources up to 1 GiB in total.
- 64-bit platforms only. Writes on Linux/Unix only.

## Validation

See [VALIDATION.md](VALIDATION.md) for exactly what is and is not covered: Rust unit
and fault-injection tests, a real-nodeos smoke test, and a real-kernel `EIO` test.
[FORMAT.md](FORMAT.md) documents the on-disk formats, plan and source files, and
upstream references.

## Development

```sh
cargo fmt --package antelope-log-repair --check
cargo clippy --locked --package antelope-log-repair --all-targets -- -D warnings
cargo test --locked --package antelope-log-repair
# Optional, Linux: a NEW development chain with real nodeos (under a minute)
python3 crates/log-repair/tests/nodeos_smoke.py --tool ./target/release/antelope-log-repair \
  --nodeos /usr/bin/nodeos --out /tmp/repair-smoke
# Optional, Linux, root: real kernel EIO through ext4 and device-mapper
sudo python3 crates/log-repair/tests/eio_smoke.py --tool ./target/release/antelope-log-repair \
  --blocks-log /tmp/repair-smoke/source/blocks/blocks.log --out /root/repair-eio
```

`.github/workflows/log-repair.yml` runs the Rust checks on Ubuntu and Windows. The
crate is not published to crates.io.

Keep this restricted parser separate from the general `hyperion-ship` indexing reader
unless their safety contracts are reconciled. Repair must reject pruning, unsupported
framing, missing anchors, and truncated files, even where an indexer could tolerate or
skip them.
