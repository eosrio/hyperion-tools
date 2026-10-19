# Real data for the `antelope-ship-archive` tests

This directory holds no log data of its own. `tests/real_slices.rs` serves real data that the
repository already publishes, in place, and reads larger real logs from directories outside the
repository.

## Committed real data

Two fixtures of the core crate, with their provenance in `crates/core/testdata/`:

- `crates/core/testdata/ship/*.bin`: seven real state-history entries from EOS mainnet and the
  WAX testnet, trace and chain_state, written compressed by Leap or as stored zlib by Spring.
  Each one becomes the only entry of a log at its own block number and is served inline and
  streamed, as v0 and v1 results. The payload must be what the strict reader inflates, of the
  size the entry declares; Spring's stored streams must also re-encode to the same bytes.
- `crates/core/testdata/blocks/eos-mainnet-480385480.hex`: EOS mainnet block 480385480. It goes
  alone into a v3 block log, beside a trace entry that names its id. The server must attach the
  block byte for byte under that id and give its `previous` as `prev_block`. `check --hash` must
  recompute the id, and an entry that names any other id must refuse startup.

## Real logs kept outside the repository

Real logs are large, and copies of private chains must not be published, so no slice of them is
committed. The ignored tests read them from the directories named below, open every file
read-only and write nothing there. Each prints what it measured and passes with a `SKIPPED` line
when its variables are not set. Run them in release mode, one at a time for clean timings:

```
SHIP_REAL_DIR=<dir> cargo test --release -p antelope-ship-archive --test real_slices -- \
    --ignored --nocapture --test-threads 1
```

| Variable | Meaning |
|---|---|
| `SHIP_REAL_DIR` | State-history logs, with or without block logs, flat or retained; sub-directories are scanned. Without block logs the server runs with `--blocks none`. |
| `SHIP_REAL_CHAIN_ID` | The chain id, needed only when no block log and no full-state first chain_state entry names it. Every source present must agree. |
| `SHIP_REAL_BLOCKS` | How many blocks are compared byte for byte (default 5000). |
| `SHIP_REAL_URL` | `ws://host:port` of a server already serving the same logs, which the full-speed run then measures instead of an in-process one. |

What they check:

- `real_logs_stream_byte_for_byte_like_the_strict_readers`:
  - The status, and every result over the first blocks (v0), the middle (v1) and the last ones
    (asked past `L*`), equal what the strict readers give when they read each file directly. This
    is checked under the default stream threshold and under a 4 KiB one.
  - Every deltas payload decodes whole as table deltas.
  - At `L*` the session idles, still answers status, and serves a new request.
- `real_logs_stream_at_full_speed`: every served block with every field, 1024 results in flight.
  The results come in order, each `prev_block` is the block before it, and the run reports
  blocks/s, MiB/s, the process's peak heap and the memory budget's peak.
- `real_logs_split_by_range_meet_at_the_seams`:
  - Three servers split the range with `--range`, and each passes `check` and reports its own
    end as head and LIB.
  - Around each seam, both sides equal one server's results but for head and LIB.
  - The first result after a seam names the last block before it as `prev_block`: with block
    logs from the block log's `previous`, without them from the state-history file holding
    that block below `--range`.
- `real_logs_first_state_entry_is_the_full_state_and_streams`:
  - The first chain_state entry (at genesis or from a snapshot) holds the full state, and its
    `global_property` row gives the chain id.
  - Treated as a large entry, it is a `check` warning (exit 2 with `--strict`), streams in frames
    of at most 1 MiB, and `--range` leaves it out.
- `real_logs_pass_a_deep_check`: `check --deep` (with `--hash` when block logs are there) finds
  nothing, reads every served entry and block, and reports its rate. Without block logs the
  warning that the first served block gets no `prev_block` is expected, unless the logs begin
  at block 2.

## Differential test against a node

### Existing running node: no copies or downtime

Run on the node's host, with read access to its non-pruned state-history and block logs:

```sh
SHIP_LIVE_NODE=ws://127.0.0.1:8080 SHIP_LIVE_DIRS=/srv/node/data \
  cargo test --release --locked -p antelope-ship-archive --test live_node existing_node \
  -- --ignored --nocapture
```

PowerShell:

```powershell
$env:SHIP_LIVE_NODE = 'ws://127.0.0.1:8080'
$env:SHIP_LIVE_DIRS = 'D:\node\data'
cargo test --release --locked -p antelope-ship-archive --test live_node existing_node -- --ignored --nocapture
```

`SHIP_LIVE_DIRS` can be a path list (`:` on Unix, `;` on Windows) when state-history,
retained logs and block logs live separately. Directories are scanned recursively. On Linux,
the test can also run in a container with the existing data directories mounted **read-only**.
No log or index is copied, moved, rebuilt or written; no links or filesystem snapshots are
created. The node keeps running. Its SHiP endpoint must serve the same chain and selected range,
with both `trace-history` and `chain-state-history` enabled.

The test selects up to `SHIP_LIVE_BLOCKS` blocks (default 200), ending just below the node's
reported LIB. It opens the original files read-only, pins their descriptors and bounds their
visible lengths at complete indexed records. New appends and renamed files cannot redirect
that fixed view. A block-log's next index offset supplies a complete block boundary, so its
actively written tail is never treated as a complete record. Logs split across retained files
work too. At most 256 selected descriptors are pinned; narrow `SHIP_LIVE_BLOCKS` if needed.

Every selected block and payload is validated with the normal strict readers. A loopback
archive forces streamed payloads and is compared with the running node: the whole selected
range with every field, then every fetch-flag combination at the range's beginning. Only head
and LIB are excluded from result equality. The archive's status must remain fixed. Set
`SHIP_LIVE_V1=1` for Spring to compare v1/finality requests too (leave it unset for Leap).
ABI text equality and the node's head/LIB before and after are printed. A missing configuration
or unavailable range fails the explicitly invoked test; it never reports a skipped run as a pass.

This is a bounded qualification test, not support for live-tailing archive serving. It relies
on nodeos preserving irreversible bytes: do not prune, vacuum, repair or overwrite these files
while it runs. Pruned formats remain refused. Capture retries up to five times over 1.5 seconds
to tolerate rotation during discovery; a persistently incomplete range is refused and may
require a rerun. Integrity errors inside the selected range still
fail. Normal `serve` and `check` retain their frozen-input contract.

The non-ignored tests in `live_node` exercise torn active tails, later appends, descriptor
pinning across rotation, truncation, corruption, irreversible limits and missing coverage.

### Frozen copy

`differential_against_a_real_node` asks a running node and this server, over a copy of that
node's logs, for the same blocks with every field. Each result must be byte-equal but for head and
LIB (bytes 1..73), and must also equal what the strict readers give. It reports whether the node's
ABI text equals the one served (serve the node's with `--abi` when it does not).

```
SHIP_DIFF_NODE=ws://127.0.0.1:8080 SHIP_DIFF_DIR=<copy> cargo test --release \
    -p antelope-ship-archive --test real_slices differential_against_a_real_node -- \
    --ignored --nocapture
```

To make the copy:

1. Stop the node.
2. Copy its `state-history` and `blocks` directories, retained files included.
3. Start the node again.

The copy then ends at a clean entry boundary, and its block logs hold only irreversible blocks,
so with `--blocks required` everything served is at or below the node's LIB. The node must still
hold the copied range.

| Variable | Meaning |
|---|---|
| `SHIP_DIFF_NODE` | The node's state-history endpoint. |
| `SHIP_DIFF_DIR` | The copy of its logs. |
| `SHIP_DIFF_CHAIN_ID` | As `SHIP_REAL_CHAIN_ID`. |
| `SHIP_DIFF_RANGE` | `<begin>-<end>`: served as `--range`, and the blocks compared. |
| `SHIP_DIFF_BLOCKS` | The most blocks compared (default 10000). |
| `SHIP_DIFF_V1` | Also compare v1 requests, which only Spring nodes answer. |
