# Real-log performance and damage qualification

Work in progress for [#46](https://github.com/eosrio/hyperion-tools/issues/46).
This document is a coverage ledger, not a completed release qualification.

The baseline is merged revision `2b4ebbcc8e9e265e995d1c46670fc79c5257cae2`.
All destructive cases use independently created disposable files. Live Libre logs
are opened read-only only to create bounded historical copies; nodeos keeps running.

## Corpus and evidence protocol

`tests/qualify.py snapshot` creates a new directory, checks indexed historical
boundaries, and copies only the selected prefixes. It hashes the copies and the
same prefixes through pinned source descriptors independently. Append-only source
growth is allowed; replaced files, shrinking prefixes, bad final trailers, hash
mismatches and insufficient capacity cause refusal. A complete `ready.json` is
published only after all selected pairs pass. The files are made read-only.

The initial selected head is Libre block **269898278**. All six prefix copies
completed, with matching independently calculated source/copy hashes:

| Kind | Log bytes | Index bytes | First block |
|---|---:|---:|---:|
| blocks | 56,409,713,202 | 2,159,186,224 | 1 |
| trace history | 104,294,471,504 | 2,159,186,216 | 2 |
| chain-state history | 162,079,017,385 | 2,159,186,216 | 2 |

These are three kinds from the same chain. They do not establish qualification on
another chain or storage system. Copying, full input hashing and cache observation
are preparation costs and are excluded from measured child-process wall times.

The attempted **128 GiB EOS/ZFS** copy was stopped after `zpool status -v`
reported a permanent data error in the selected original block log. Its partial
copy and diagnostics remain; no complete corpus manifest was published. A smaller
1 GiB prefix had independently matching source/copy hashes. It supplies useful
parser fixtures, including a block with 128 transactions, but its timing results
are diagnostic observations under a reported pool error, not healthy-ZFS evidence.
The tool did not modify the original, clear errors, or initiate storage repair.

A separate **64 GiB Ultra/ext4/NVMe** prefix completed independent source/copy
hashing: blocks 1–192881743, 68,719,476,814 log bytes and 1,543,053,944 index bytes.
Repeated baseline/candidate `scan` and `verify` measurements are in progress.
The 10,000-block preflight passed those indexed commands but tail recovery refused
Ultra's different genesis layout: its first position is 127, versus 126 for the
supported v3 classic genesis. No heuristic preamble search or format relaxation
was added. This is indexed-command coverage and an explicit recovery refusal,
not full recovery qualification for Ultra. Other pruned logs were excluded.
Linux page-cache residency does not measure ZFS ARC residency.

```sh
python3 crates/log-repair/tests/qualify.py snapshot \
  --blocks /node/blocks --ship /node/state-history \
  --through-block HISTORICAL_BLOCK --source-revision COMMIT \
  --out /scratch/new-corpus
```

## Measurements

The benchmark verifies independent input hashes before timing and checks file
identity, size, modification/change times and binary identity around each command.
Inspection results must match the full manifest range and hashes; verify cannot
pass by skipping oversized entries. Failed runs remain in the evidence directory
and no successful completion marker is written for an incomplete batch.

Per-child `wait4` captures CPU, faults and maximum RSS. The latter can include
fork-before-exec launcher memory, so exe-matched `/proc` VmHWM samples are retained
separately. I/O counters are last-sample lower bounds, not exact final totals.
Unavailable kernel metrics are reported independently. Named phase observations
are polled, with the sampling interval recorded; they are not exact internal spans.

Cache residency is measured with bounded `mincore` mappings. The optional
`file-dontneed` mode advises only the disposable corpus descriptors; it never
flushes the host cache. DONTNEED is advisory, so the actual resident page fractions
and physical I/O evidence must support any cold-cache interpretation. Ordinary
repeats are not automatically described as fully warm.

```sh
python3 crates/log-repair/tests/qualify.py benchmark \
  --tool /scratch/baseline --corpus /scratch/new-corpus \
  --source-revision COMMIT --kinds blocks trace_history chain_state_history \
  --commands scan verify inspect --repeats 3 --out /scratch/new-baseline
```

`--profile strace` and `--profile perf --perf /path/to/perf` are separate diagnostic
workloads. Their elapsed time must not be
compared to an uninstrumented result without measuring profiler overhead. Native
CPU profiles use a separately identified build with debug symbols; they do not
replace timing the ordinary release artifact.

### First measured optimization: decoder workspace reuse

On x670e-7950x (Ryzen 7950X, Ubuntu 22.04, ext4/NVMe), the baseline's complete
104.3 GB trace-history verification took **2,753.83 s**. Three decoder-reuse runs
took **421.37 s median (418.98–431.90 s)**, about 6.5 times faster. Every report
field except elapsed time matched: 269,898,277 entries and 104,294,471,504 bytes.
The baseline was a single pilot on a running host, overlapping preparation and
profiling work; it is not a three-versus-three controlled full-log comparison.

A separate 30-second user-CPU profile of canonical SHIP inspection attributed
28.05% of samples to `memcpy`, 24.95% to inflater Huffman-tree initialization,
15.21% to `memset`, and 5.19% to allocator/free functions. It is a partial-prefix
diagnostic, not completed validation. User-only sampling omits kernel CPU; the
full verification also spent substantial time in the kernel.

The first change reuses flate2's inflater and a 64 KiB scratch buffer per decoding
thread. Every stream uses the library's complete reset, including after errors.
Returned payloads remain owned, and validation reads/checksums remain unchanged.
The diagnostic `allocation-metrics` Cargo feature counts Rust allocation requests;
it is excluded from ordinary builds and does not measure C allocations or RSS.
Build it explicitly with `cargo build --release -p antelope-log-repair --features
allocation-metrics`; it writes one `allocation-metrics:` JSON line to stderr
after the operation. Keep this artifact's timing separate from the ordinary build.

Three ordinary release runs of the **same 1,000,000 trace entries**, blocks
209394282–210394281 (457,341,378 indexed bytes), produced:

| Artifact | Child wall median (min–max), s | Tool operation median, s |
|---|---:|---:|
| Baseline | 9.544 (9.540–9.544) | 9.198 |
| Decoder reuse | 1.030 (1.021–1.030) | 0.542 |

Every verification JSON field except elapsed time matched, including the 11
framed orphaned fork entries. The process sampler in this frozen harness version
polled every 0.5 s, so child wall time includes up to one polling interval. Later
versions default to 0.05 s and retain both measurements. These are ordinary-cache
range results, not a full-log speedup or a cold-cache measurement.

One allocation-instrumented run per artifact counted **2,000,674 → 1,000,676**
allocations and **44,164,136,249 → 868,245,065** requested bytes. These single
diagnostic runs establish allocation reduction, not profiler timing overhead.
The ordinary artifacts are pinned by SHA-256:

- Baseline: `58efe37f33f9a0cd3687422db043012d6c91db87e769400508559f05d80346b9`.
- Decoder reuse: `97f5c3f8ed46d46035c4404ab1005db82c1b82ca01f025716825f0e01bf587f3`.

A second, separately measured change validates the complete decompressed stream
without collecting decoded bytes when a caller only needs validation and its ID.
Payload-returning callers keep their owned buffers. Both paths share framing,
checksum, exact-consumption and expansion-limit checks, with matching errors.
In three interleaved runs, the modern million-entry operation median changed from
0.542 s to 0.494 s; the early range changed from 2.357 s to 2.309 s. The latter is
a small difference and should not be treated as a large throughput improvement.

On a real 139,207,634-byte initial-state payload plus 1,997 following deltas,
the operation median changed from 0.110 s to 0.070 s. Per-child `wait4` peak RSS
was 274,308 KiB versus 157,208 KiB (about 43% lower). The launcher-memory caveat
above applies; exe-matched VmHWM samples missed some short-lived peaks. Canonical
IDs were checked against the frozen block log, and all report fields except time
matched. The first initial-state payload is not claimed equal to the historical
incremental delta.

The first candidate also fixes a reproduced `verify` panic from overflowing
SHIP entry lengths. A release binary wrapped a malformed length to zero and
panicked while slicing its trailer. Saturating the computed extent keeps it out
of bounds, so verification reports corruption. Boundary cases are tested in both
SHIP kinds. Candidate SHA-256:
`bcaed15639cb0e9338d1bee4093132c5815e51195852f1bbf8f382765a1d3dc5`.

The full baseline tail inspection found another real case: after a fork, Libre
repeats canonical entries 172018829–172018840 with byte-identical encoded bodies.
It stopped at offset 55,616,016,557 with a complete prefix ending at 172018840;
the benchmark oracle correctly rejected that as incomplete coverage. This was
not a timeout or a successful inspection of the full trace log.

The follow-up candidate retains at most 4 MiB of canonical SHIP bodies over
4,096 blocks. With canonical block evidence, exact repeated bodies can be omitted;
an index pointing to either identical occurrence is valid. Changed payloads,
different encodings, invalid checksums, and repeats outside the bounds keep the
existing ambiguity stop. Regression tests exercise both SHIP kinds, missing and
partial indexes, wrong positions and cache eviction. The new artifact was built
on Windows for Linux/musl after WSL stopped responding, so it has separate build
provenance and is not substituted into the earlier measured experiments. SHA-256:
`ca5294ae4719e97f0d16ceffe272b09a304e4c39f35f3a8d63ddfe1067661381`.

The [structured checkpoint](../../docs/performance/evidence/log-repair-20261019/checkpoint.json)
retains per-run CPU/RSS/I/O, allocation counts, input and binary hashes, and scope
limits. It is explicitly an unfinished qualification checkpoint. Strace confirms
the first change removes roughly one `mmap`/`munmap` pair per entry on these musl
builds; this does not predict the same speedup on another allocator or platform.

For subsequent same-host alerts, use the greater of 5% or twice the baseline
relative min/max spread as the initial median CPU-time regression allowance.
Repeat an exceeded band under comparable cache/load conditions before attributing
it to code. This is a provisional investigation budget, not a universal CI timing
gate. Profiling overhead remains workload-specific: paired allocation diagnostics
were 0.560 s versus 0.543 s on the decoder-reuse modern range; full native/strace
overhead qualification remains pending.

## Damage and acceptance work

`extract` selects up to 10,000 real blocks into a new fixture. The matching
`spring-util` independently extracts/rebases the block log, whose bodies are then
compared byte for byte with the frozen source. The Python oracle calculates header
IDs and checks their chain, selects canonical SHIP entries with matching IDs,
checks zlib streams independently, and records each body's hash and original/new
position. Physical SHIP gaps are explicitly recorded and excluded. This does not
authenticate signatures, Merkle roots or SHIP semantics.

```sh
python3 crates/log-repair/tests/qualify.py extract \
  --corpus /scratch/new-corpus --utility /usr/bin/spring-util \
  --first FIRST --last LAST --out /scratch/new-small-fixture
```

The full issue remains open. This ledger separates acquired evidence from remaining
release work; individual measured pilots do not qualify an entire family.

| Family | Acquired evidence | Remaining or deliberately refused |
|---|---|---|
| Performance | Repeated SHIP decoder experiments, full trace verification, native sampling, allocation/RSS and syscall evidence; separately timed full block recovery pilot | Repeat every command/operation on the final artifact; controlled cache and profiler overhead; healthy ZFS/storage diversity |
| Torn tails and indexes | Real-body EOF, framing, trailer, zeros/garbage and index mutations; exact copy-out and trim/rollback oracles | More transaction/extension/signature layouts and large damaged entries; no-anchor and unsupported preambles remain refused |
| Readable corruption | Header/body/checksum edits and an interior zeroed entry, with reference comparison | Broader multi-extent/gap combinations; signature, Merkle-root and SHIP semantic authentication are outside the validator |
| SHIP and sources | Mixed encodings, large initial state, forks, byte-identical canonical repeats, donor disagreements, missing evidence and source-change tests | Conflicting/differently encoded repeats stop; older/oversize repeats stop; broader writer/version and large donor combinations |
| Media failures | Private kernel EIO read detection and failed-write refusal with intent preservation | Successful healing, ENODATA and multiple actual error extents; readable EOF damage is a separate workflow |
| Interrupted recovery | Actual process kills at truncate boundaries, injected ENOSPC and deterministic journal/rollback tampering tests | Other actual write/fsync/short-write boundaries and VM/storage power cuts |
| Acceptance | Matching utility smoke test and isolated Libre snapshot replay with head and SHIP payload oracles | Other versions/snapshots and current-network catch-up; catching up blocks does not recover missing historical SHIP |

No release tag is qualified by this checkpoint. Simulated interruption states,
actual process termination, syscall fault injection and storage power cuts remain
distinct evidence classes.

Completed evidence so far:

- **174 deterministic cases** over eight real Libre block/trace/delta bodies:
  torn and aligned tails, zeros/garbage, malformed preambles, index variants,
  readable header/signature/checksum damage, an interior zeroed entry and canonical
  repeats with first/latest index positions or conflicting payloads/encodings.
  Restore/salvage outputs use independent byte/hash oracles; eligible trims have
  exact saved-byte and rollback checks. The latest candidate passed on the real
  server; the earlier 158-case candidate also passed on Linux/WSL. These include
  malformed-length findings from `verify`.
  This small fixture does not establish large-entry or storage power-loss behavior.
- A separate **26-case EOS block matrix** passed, including an EOF cut inside
  block 332255806's two-byte count of 128 transactions. The three committed real
  bodies and independent header-ID oracles came from the hash-verified 1 GiB prefix.
- All **92 Linux Rust tests** passed with allocation instrumentation, including
  mixed compressed/stored streams, decoder-error recovery, overflow guards and
  bounded canonical repeats. Native Windows passed Clippy, 67 ordinary Rust tests
  and the allocation-counter test. All 20 Python harness tests passed on Linux;
  Windows passed the three portable cases and explicitly skipped 17 Linux cases.
- Four actual tracee SIGKILL cases around the two truncate calls passed resume
  and exact rollback. Five injected ENOSPC cases covered pre-intent fsync,
  truncation and rollback writes. Linux pidfds constrain signals to owned children.
  These are syscall/process tests, not physical storage power cuts.
- A kernel EIO test on a private ext4/device-mapper image found the expected
  unreadable page, built from the intact donor, and refused a write-failing apply
  while keeping its intent and omitting a receipt. Cached data masked the failed
  write until that private filesystem was remounted. No host cache flush occurred;
  the private mount, mapper and loop all cleaned up successfully. Successful media
  healing, ENODATA and multiple real error extents remain separate work.
- Indexed-size measurement covered all 269,898,278 block entries. The largest was
  237,597 bytes (block 264131611); systematic sampled p50/p95/p99 were
  192/335/392 bytes. Size alone does not establish transaction density.
- A disposable 2,000-block slice was independently extracted by Spring. After
  tearing its last trailer and journal-trimming it, `spring-util` smoke-tested
  the resulting pair. An isolated nodeos replay from the matching snapshot
  reached block **209396280**, with independently matching head ID and clean
  shutdown. All 1,998 regenerated trace payload hashes matched. Of the delta
  payloads, the first replayed block contained a 139,207,634-byte initial state;
  the remaining 1,997 matched. The initial state is deliberately not counted as
  equal to the original 629-byte historical delta. Spring emits full state into
  a [fresh chain-state log](https://github.com/AntelopeIO/spring/blob/9f6e4496db1ebffec088f9206b645ef9571f2889/plugins/state_history_plugin/state_history_plugin.cpp#L175-L183).
  With an existing canonical history anchor, all 1,998 newly replayed trace and
  delta payloads matched, in addition to the seeded record. This is historical
  snapshot replay on disposable files, not current-network catch-up.

Recovery operations can be timed separately on new copies of the frozen block log:

```sh
python3 crates/log-repair/tests/qualify.py mutate \
  --tool /scratch/candidate --corpus /scratch/new-corpus --repeats 3 \
  --out /scratch/new-mutation-benchmark --retire-data
```

Copy/hash preparation and independent output hashing are outside the measured
trim, no-op resume, rollback, salvage and restore processes. By default every data
file remains. `--retire-data` removes only this invocation's individually verified
log/index copies after their oracles pass; journals, receipts and hashes remain.
Changed files, hard links and paths outside the fresh run directory refuse
retirement. Cleanup also rehashes each file immediately before retirement: a real
ext4 run demonstrated that same-length edits can share observed timestamps.
The regression deliberately matches metadata while retaining a stale byte proof.

A three-repeat real-entry smoke passed. The full **56.4 GB pilot** completed with
the first candidate and independently matching recovery/rollback bytes:

| Operation | Seconds (one full-size run) |
|---|---:|
| In-place trim | 165.148 |
| No-op resume | 35.387 |
| Rollback | 71.301 |
| Copy-out salvage | 287.670 |
| Copy-out restore | 1,090.717 |

These are individual pilots, not repeated medians or a cross-version speedup claim.
The selected original corpus remained read-only. Only disposable files whose
independent oracles passed were retired; journals and hashes remain.

The opt-in interruption matrix uses `strace` 5.16 or compatible syscall injection:

```sh
python3 -O crates/log-repair/tests/qualify.py interrupt \
  --fixture crates/log-repair/tests/fixtures/libre-real-entries.json \
  --tool /scratch/candidate --out /scratch/new-interruption-tests
```

Fast harness refusal and measurement tests run with:

```sh
python3 -m unittest discover -s crates/log-repair/tests -p test_qualification.py
```

The Linux-specific cases are skipped on Windows. All destructive preconditions use
explicit exceptions and remain enabled under optimized Python.
