# antelope-log-repair

Finds and repairs **unreadable records** inside Antelope `blocks.log`,
`trace_history.log`, and `chain_state_history.log` files, including retained range
files, without resyncing the node.

When a disk or filesystem returns an I/O error for part of a log, nodeos can no
longer serve those blocks. This tool locates the unreadable bytes and rebuilds the
affected entries from independent sources. It proves that they fit the surrounding
readable data, and it writes back only the unreadable bytes, after you approve a
reviewed plan.

For a power loss during an append, a truncated final entry, or a missing/torn index,
use [Recover a crash-damaged tail](#recover-a-crash-damaged-tail). This separate
workflow shows restoration and salvage choices and stages a **new log/index pair**.
It preserves the damaged originals. Linux also has an explicit
[suffix-only in-place trim](#trim-a-small-tail-in-place-linux) with saved bytes,
a recovery journal, resume, and rollback.

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
[Recover a crash-damaged tail](#recover-a-crash-damaged-tail) ·
[Trim a small tail in place](#trim-a-small-tail-in-place-linux) ·
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

The in-place media workflow repairs bytes the operating system **refuses to read**:
reads that fail with `EIO` or `ENODATA`. Only those errors authorize a write into the
original log through `apply`. The separate `recover-tail` workflow writes new files;
`trim-tail` explicitly shortens a readable block-log pair after saving both suffixes.

| Situation | Supported? |
|---|---|
| ZFS permanent data error (checksum/IO) on a log file | Yes: the design target. Healed real on-disk damage (see [Validation](#validation)) |
| btrfs checksum error on a log file | Yes, with 4 KiB records. Healed real checksum damage; see [After the repair](#5-after-the-repair) for scrub |
| ext4/xfs on a disk with unreadable sectors | Yes, with 4 KiB records, the filesystem default. Healed real sector errors on dm-integrity (see [Validation](#validation)) |
| Silent corruption: the bytes read back, but wrong | Media writes are refused. [`verify`](#check-for-silent-corruption) detects it; `recover-tail` can rebuild a suffix into new files from trusted sources |
| Damage in the first entry, the head (last) entry, or the file header | Media repair requires neighbors on both sides. `recover-tail` handles a readable damaged suffix after a complete anchor; invalid preambles/first entries are refused |
| Readable damaged or missing `.index` | `recover-tail` reconstructs an index in new files from checked entries; the media workflow still requires an intact index |
| SHIP damage inside orphaned fork entries (abandoned entries nodeos leaves after a fork) | No: no other node, endpoint, or replay holds those bytes. Damage next to them is repaired, and they are kept as they are (see [FORMAT.md](FORMAT.md#target-logs)) |
| Pruned block logs, SHIP logs with feature flags | No: refused as unsupported formats |
| SHIP logs written by EOSIO 2.1 or 2.2 (state-history log version 1) | No: their trace entries split out prunable transaction data that SHIP does not serve. Refused by name |
| State history from nodeos 1.5 to 1.7 (no `ship` magic) | No: an older header format. Refused by name |
| A truncated log, or readable damage at the end of the file | [`recover-tail`](#recover-a-crash-damaged-tail) stages restored or explicitly salvaged files; Linux `trim-tail` can instead save and remove a small suffix in place |

It runs on Linux and other Unix systems. On Windows, read-only inspection, source
staging, bundle building, and copy-out tail recovery work; in-place writes are disabled.

### Logs written by several nodeos versions

Upgrading nodeos never rewrites a log: a log started on an older release and now
written by Spring keeps each part as it was first written. The tool works out how each
damaged part was written from the log itself, so it does not need to know your history.

- **`blocks.log`:** non-pruned versions 1, 2 and 3 are supported. Their headers
  differ; recovery preserves the original header and version. Classic genesis
  layouts and Ultra mainnet's exact genesis are recognized (see [FORMAT.md](FORMAT.md#target-logs)).
  Any intact copy, any retained `blocks-FIRST-LAST.log` file, or any state-history
  endpoint can supply the blocks.
- **Finding damage** (`scan`, `verify`) works on every part of a log.
- **SHIP logs:** each release wrote its entries in its own encoding:

  | Writer | Payload | Preamble |
  |---|---|---|
  | EOSIO 1.8, 2.0; Leap 3.x; forks built on them | zlib, compressed at level 6 | u32 compressed length |
  | Leap 4.0 to 5.0, and forks built on them (for example WAX, UX) | zlib, compressed at level 6 | `1`, then the u64 uncompressed size |
  | Spring 1.0 and later, and forks built on it (Spring CE, Wire) | zlib, stored, 32 KiB blocks | `1`, then the u64 uncompressed size |

  Endpoints, donor logs, and replays all supply the decompressed payloads, and the
  tool **re-encodes them the way your log was written**. A long-lived log usually holds
  both compressed and stored stretches; each region is handled on its own.

**How the encoding is found.** For each damaged region the tool reads the two readable
anchor entries around it and tries every encoding it knows:

- stock zlib at levels 1 to 9;
- stored zlib in 32,768-byte blocks (zlib 1.2.11 and later) or 32,506-byte blocks
  (older zlib), each with or without an empty final block when a payload fills its last
  block exactly.

It keeps only the encodings that reproduce **both anchors byte for byte**. The rebuilt
entries must then match every index position and every readable byte of the region,
and exactly one distinct result may fit. A donor whose entries are already identical to
yours is used as it is. `build` prints, for each region, either `source bytes as they
are` or `re-encoded as …`.

**What can stop it:**

- **The node was upgraded inside the damaged region,** so its anchors differ (for
  example the block where a node moved to Spring). Refused: `different encodings`.
- **The node's zlib was not stock zlib,** so no candidate reproduces the anchors.
  zlib-ng (the system libz on some distributions and containers), Intel's or Cloudflare's
  forks, and s390x hardware compression produce other bytes; every official nodeos
  release links the host's `libz.so.1`. Check with `ldd $(which nodeos)` and the version
  of that library on the host that wrote the log. Refused: `no known encoding
  reproduces anchor`. Use a donor whose entries are byte-identical instead.
- **The writer's payloads differ,** for example a different `trace-history-debug-mode`,
  or replay with a release that serializes traces differently. The readable bytes then
  differ, and the region is refused.

The tool encodes with stock zlib compiled into the binary, never with the zlib of the
machine it runs on.

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

SHIP endpoints send uncompressed history payloads, not disk entries. The fetch saves
them as a stored-zlib donor, and build re-encodes them the way your log was written,
compressed or stored (see
[Logs written by several nodeos versions](#logs-written-by-several-nodeos-versions)).
If they still do not fit, for example because of a different debug setting or a
non-stock zlib on the original host, the fetch fails and keeps its files and report
for diagnosis, and build refuses them. Use replay, or a donor log with identical
entries.

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

Packed blocks contain no traces or deltas. If no endpoint or copy holds the history,
regenerate it from a snapshot taken before the damage and the blocks after it:

```sh
antelope-log-repair prepare --log /data/state-history/chain_state_history.log \
  --damaged-block 1234567 \
  --nodeos /opt/spring/bin/nodeos \
  --snapshot /snapshots/snapshot-1230000.bin --snapshot-block 1230000 \
  --blocks-dir /data/blocks --blocks-dir /data/blocks-retained \
  --chain-state-db-size-mb 32768 \
  --out /scratch/repair-1
```

- The snapshot must be an uncompressed portable snapshot (format 1). It must be taken
  **at most two blocks before the first damaged block**: the first replayed block
  emits a full-state delta, which must precede the damage. `scan` prints the latest
  allowed head.
- `--snapshot-block` is the head you verified when obtaining the snapshot. The tool
  does not read it from the snapshot; nodeos validates the snapshot contents.
- `--blocks-dir` (repeatable) names directories holding the intact block history: a
  `blocks.log` and any retained `blocks-FIRST-LAST.log` files, each with its index, for
  example your node's `blocks-dir` and `blocks-retained-dir`. Together they must cover
  the snapshot head through the last damaged region, without gaps, and belong to one
  chain. The tool copies **only that range** into a fresh blocklog in the
  sandbox, so space and time grow with the distance from the snapshot to the damage.
  Legacy sources use v2 slices with their genesis; v3 sources use v3 slices.
  Pick a recent snapshot.
- The replayed payloads are re-encoded the way your log was written, so a current
  release can replay a stretch that an older one compressed, as long as it produces
  the same payloads. That has been tested with Spring replaying Spring-era history
  only; see [Validation](#validation).
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
  including both anchors; its `.index` must sit alongside. Its file offsets and its
  encoding may differ: only the payloads are used, re-encoded the way your log was
  written when its own bytes do not fit.
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
- the `entries:` line of each region: `source bytes as they are`, or the encoding the
  payloads were re-encoded with;
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

## Recover a crash-damaged tail

`recover-tail` reads physical entries independently of the target's index. It can
inspect a missing, empty, truncated, stale, or incorrect index, a partial final
header/payload/trailer, trailing zeros, and other readable corruption. It stops at
the **first invalid or unsupported entry** and labels the remaining bytes
unclassified; it does not assume corruption is confined to the last entry.

Start with read-only inspection:

```sh
antelope-log-repair recover-tail --log /data/blocks/blocks.log
```

The report shows the complete prefix, first rejected offset and reason, index
discrepancies, and **both** recovery choices. Restoration is preferred. To restore
the suffix and regenerate the index from trusted local block data:

```sh
antelope-log-repair recover-tail --log /data/blocks/blocks.log \
  --blocks-log /backup/blocks/blocks.log --through-block 1234567 \
  --out /scratch/tail-restored --node-stopped
```

- `--through-block` is the intended final block verified by the operator. A torn
  file cannot prove how many entries were never written. Restoration refuses an
  end before the complete prefix or the index's head hint, missing source coverage,
  conflicting sources, or an ID chain that does not join the prefix.
- The default `--mode restore` **never falls back to salvage**. If restoration
  lacks data, inspection still shows why, and staging refuses before creating output.
- `--blocks-log`, `--blocks`, `--donor-log`, and `--sources` accept the same local
  source types as `build`. Block sources may be combined; overlapping packed blocks
  must agree. SHIP donors may use different encodings, but their decompressed
  payloads must agree throughout the retained and restored range. Where a donor
  covers the retained prefix, its payload must also match the target. Donor logs
  and block logs still need usable indexes.

For SHIP, supply canonical block evidence covering the prefix and restoration
range, plus a trusted donor of the target kind for missing payloads:

```sh
antelope-log-repair recover-tail --log /data/state-history/trace_history.log \
  --blocks-log /backup/blocks/blocks.log \
  --donor-log /backup/state-history/trace_history.log --through-block 1234567 \
  --out /scratch/trace-restored --node-stopped
```

Physical SHIP entries alone cannot identify the canonical fork. With block evidence,
the tool omits intact noncanonical entries and rebuilds canonical positions; without
it, SHIP inspection is provisional and output is refused. A final unindexed entry
is examined instead of being silently assumed to be an abandoned fork.

After a fork, SHIP may repeat an earlier canonical entry. Recovery can omit a repeat
whose entire encoded body equals the earlier validated body, with canonical block
evidence for both. These repeats are included in `omitted_fork_entries`. An index
pointing to either identical copy is accepted. The comparison retains at most
4 MiB of bodies across 4,096 blocks; older or larger repeats, conflicting payloads,
and different encodings still stop inspection as ambiguous.

To keep only the complete prefix, select salvage explicitly:

```sh
antelope-log-repair recover-tail --log /data/blocks/blocks.log \
  --mode salvage --out /scratch/tail-salvaged --node-stopped
```

SHIP salvage also needs `--blocks-log` or `--blocks`. Review the excluded suffix
bytes and missing declared/indexed block ranges. Without a verified intended head,
the total missing history is unknown. Catching up blocks after restarting nodeos
does not itself regenerate old SHIP traces or deltas; restoring that history may
require a donor or replay.

**Output and activation.** Stop the target node and every writer before staging.
The NEW `--out` directory holds `recovery.json`, `recovered/LOG.log`,
`recovered/LOG.index`, and, only after flushing, syncing and verifying the generated
pair byte for byte, `complete.json`. The reports record input fingerprints, source
paths/stamps, selected mode, output hashes and validation limits. Read-back can be
served by the filesystem cache. Directory fsync is supported on Unix; Windows
staging does not establish power-loss durability of directory entries.

Source reads use the same open file handles that are pinned and checked for
identity changes. Each target pass re-reads its preamble and verifies the input
fingerprint before accepting completion.

If staging fails or is interrupted, keep its incomplete output for investigation;
an existing output directory is never reused. Require a valid `complete.json` and
matching output hashes before considering activation. Activation is manual and
must coordinate both files with nodeos stopped: two separate file renames do not
make an atomic pair. Keep the damaged originals until node acceptance is verified.

This recovery path supports non-pruned block-log v1/v2/v3 and SHIP v0, requires at
least one complete anchor entry, and bounds a single entry and the restored suffix
to 256 MiB, with at most 10,000 restored entries and one million elements per FC
vector. The prefix is streamed, with bounded read-ahead. It reads the whole log and
index during inspection, repeats validation while staging, and reads the output
back. Progress names the three phases: inspect original (1/3), write recovered
pair (2/3), and verify recovered pair (3/3). Each phase starts its own byte counter.
Allow space for the new pair. It checks complete outer `signed_block` framing,
header links and SHIP checksums; it does not authenticate transaction bodies,
signatures, action roots, or SHIP payload semantics. Use trusted independent sources.

Direct endpoint acquisition and snapshot replay against a broken target, automatic
activation of staged pairs, recovery without any anchor, and salvage past an interior
gap are not implemented. Local donor files produced by a controlled replay can be supplied.
Media read errors remain on the existing `scan`/`prepare`/`apply` path.

## Trim a small tail in place (Linux)

When restoration is unavailable and the operator chooses to keep the complete
prefix, `trim-tail` avoids copying the whole log. **It modifies the existing log
and index.** Stop nodeos and every reader/writer first, keep them stopped through
completion, and run as root so `/proc` can inspect other users' file descriptors:

```sh
antelope-log-repair trim-tail --log /data/blocks/blocks.log \
  --journal /scratch/tail-trim-1 --node-stopped
```

This supports non-pruned block-log v1/v2/v3 with at least one complete anchor block.
The existing index must contain every retained slot and agree with the checked
physical entries; extra whole or partial slots may be removed. **It trims an
already correct index prefix; it does not rebuild a missing or inconsistent index.**
Use copy-out `recover-tail` for those cases, SHIP logs, media read errors, or more
than 256 MiB of combined excluded log/index bytes. A clean pair is refused as a
no-op. `--index PATH` selects a non-default index.

The tool inspects and hashes the complete input pair, saves exactly the bytes it
will remove, commits the journal, truncates and fsyncs the log, then truncates and
fsyncs the index. It reads the resulting pair back through the full parser and
writes `receipt.json` last. The retained prefix is never rewritten. Initial trim
uses two complete passes, with named progress phases. Read-back can use cache;
this does not establish that the storage hardware honors flushes on power loss.

The NEW journal directory contains `log.tail`, `index.tail`, `intent.json`,
`intent.sha256`, and `lock`. The intent binds absolute paths, device/inode identity,
original and retained lengths, block range, and whole/prefix/suffix SHA-256 hashes.
Keep the journal private and retain it until node acceptance is established.
Checksums detect changes; they do not authenticate a journal rewritten by its owner.
The tool locks both targets and the journal, checks for other openers and path
replacement, and checks target modification/change times around its work.

If interrupted after the intent checksum is committed, keep both files at their
recorded paths and resume with nodeos still stopped:

```sh
antelope-log-repair resume-tail-trim --journal /scratch/tail-trim-1 --node-stopped
# Restore the exact saved, damaged originals instead:
antelope-log-repair resume-tail-trim --journal /scratch/tail-trim-1 \
  --rollback --node-stopped
```

Resume hashes both retained prefixes and accepts only the original or trimmed
length of each file. Rollback appends the saved index suffix and then the log
suffix; an interrupted partial append can resume only when every byte matches the
saved suffix. Once rollback starts, continue with `--rollback`; restarting trim
from that journal is refused. Completed operations are verified without rewriting
the files. Changed prefixes, unexpected lengths, replaced files, invalid journal
artifacts, or another opener cause refusal. If preparation stops before a complete
`intent.sha256` exists, no truncation has started; inspect that incomplete journal
and use a new directory. Do not edit or discard a committed journal to bypass a
refusal.

Rollback restores the damaged input, including its torn entry and stale index.
Trimming does not recover missing blocks or SHIP payloads, and the index head is
only a hint about missing history. Prefer restoration from trusted sources where
possible. After a successful trim, qualify the pair with the matching node utility,
then check node startup/replay and history coverage before returning it to service.

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
| `touches the log's first or head entry` | No readable entry on one side for in-place media repair | For a readable damaged tail, use `recover-tail`. It still requires a complete prefix anchor |
| `overlaps the log header` / `reaches past the last entry` | Damage in the file header or tail | A readable tail can use `recover-tail`; a damaged/unsupported preamble is refused |
| `byte N near this damage is unreadable too` | An anchor or readable gap also fails | Scan a wider range or the whole file so all nearby damage is in one plan |
| `stopped after N failed reads` | Widespread damage | Check the device and pool health. Raise `--max-bad-reads` only if you are sure |
| `this platform reports no filesystem block size` | Windows, or an unusual filesystem | Pass `--record-size` |
| `followed by orphaned fork entries … reaches them` | The damage covers entries nodeos abandoned after a fork, which no source holds | Not repairable. Narrow the selection to the other regions; for these bytes, resync or restore the log |
| `entry N length … differs from target` (with no encoding that fits) | Neither the source's bytes nor any re-encoding fits this entry: the payload itself differs, for example another debug setting or node version | Use a donor from a node with the same configuration, or replay with matching settings |
| `anchors … were written with different encodings` | The node was upgraded inside this region (for example to Spring) | Not supported. Use a donor whose entries are byte-identical |
| `no known encoding reproduces anchor block N` | The log was written with a non-stock zlib, such as zlib-ng | Check `ldd $(which nodeos)` on the writing host; use a donor whose entries are byte-identical |
| `several encodings reproduce the anchors and fit` | The damaged bytes cannot tell the candidates apart | Not supported. Use a donor whose entries are byte-identical |
| `state-history log version 1, written by EOSIO 2.1 or 2.2` / `written before nodeos 1.8` | An unsupported log format | Not repairable with this tool |
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
| `recover-tail` | Inspect crash tails/indexes; restore or explicitly salvage to new files | Nothing without `--out`; otherwise a new staging directory |
| `trim-tail` | Save and remove a small readable block-log/index suffix (Linux) | Existing log/index lengths and a new recovery journal |
| `resume-tail-trim` | Finish a journaled trim or append saved bytes with `--rollback` | The recorded pair and journal completion markers |
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

- Formats: non-pruned block-log v1/v2/v3 (including retained range files), and non-pruned
  SHIP v0, including legacy payload size markers and Leap 4+ uncompressed-size headers,
  compressed or stored. Unknown features fail closed.
- Record size: a power of two from 512 B to 16 MiB.
- Per plan: up to 64 regions and 256 MiB of damaged bytes.
- Per region: up to 9,998 affected blocks and 256 MiB of affected entries.
- 256 MiB caps on a single entry, a per-endpoint SHIP transfer, and a decompressed
  payload; block sources up to 1 GiB in total.
- 64-bit platforms only. In-place media writes on Linux/Unix only; copy-out tail
  staging also works on Windows, with the directory-durability limitation above.

## Validation

See [VALIDATION.md](VALIDATION.md) for exactly what is and is not covered: Rust unit
and fault-injection tests, a real-nodeos smoke test, and a real-kernel `EIO` test.
[FORMAT.md](FORMAT.md) documents the on-disk formats, plan and source files, and
upstream references. [PERFORMANCE.md](PERFORMANCE.md) records real-log tail-inspection
measurements and their cache and hardware limits.
[QUALIFICATION.md](QUALIFICATION.md) tracks real-corpus performance, damage,
process-interruption and node-replay work for the next release, including measured
results and remaining gaps.

## Development

```sh
cargo fmt --package antelope-log-repair --check
cargo clippy --locked --package antelope-log-repair --all-targets -- -D warnings
cargo test --locked --package antelope-log-repair
# Optional, Linux: a NEW development chain with real nodeos (under a minute)
python3 crates/log-repair/tests/nodeos_smoke.py --tool ./target/release/antelope-log-repair \
  --nodeos /usr/bin/nodeos --out /tmp/repair-smoke
# Optional: torn copies of that development chain, then real-nodeos replay of recovered files
python3 crates/log-repair/tests/tail_smoke.py --tool ./target/release/antelope-log-repair \
  --source-smoke /tmp/repair-smoke --nodeos /usr/bin/nodeos --out /tmp/repair-tail-smoke
# Optional, Linux: time read-only inspection of a stable retained log or frozen copy
python3 crates/log-repair/tests/tail_bench.py --tool ./target/release/antelope-log-repair \
  --log /scratch/frozen/blocks.log --out /tmp/tail-benchmark \
  --cache-note "ordinary page cache; no global cache eviction"
# Optional, Linux, root: real kernel EIO through ext4 and device-mapper
sudo python3 crates/log-repair/tests/eio_smoke.py --tool ./target/release/antelope-log-repair \
  --blocks-log /tmp/repair-smoke/source/blocks/blocks.log --out /root/repair-eio
```

`.github/workflows/log-repair.yml` runs the Rust checks on Ubuntu and Windows. The
crate is not published to crates.io.

Keep these restricted parsers separate from the general `hyperion-ship` indexing
reader unless their safety contracts are reconciled. In-place media repair rejects
pruning, unsupported framing, missing anchors, and truncated files. `recover-tail`
may inspect an inconsistent index or truncated suffix, but emits only entries whose
complete framing has been checked and reports all excluded bytes.
