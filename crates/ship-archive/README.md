# antelope-ship-archive — frozen state history, served as a Spring node serves it

Serves **frozen** Antelope state-history logs (`trace_history`, `chain_state_history`,
`finality_data_history`) and block logs over the **SHiP WebSocket protocol**, exactly as a
Spring node serves them, without nodeos. It reads flat and retained (split) files in place,
read-only.

It is the archive tier of a range-split history. Each server holds one contiguous range of
irreversible blocks copied off a node, and reports its last block as both head and LIB. A
SHiP-aware router or proxy stitches such servers and the live nodes into one history for its
clients, so readers of old history no longer load the live nodes. An archive server needs
disk and a little CPU: no chain state, no replay, no nodeos.

Part of the [Hyperion Tools](../../README.md) Cargo workspace, MIT licensed
([LICENSE](../../LICENSE)). Status: **preview**. Its wire behavior is pinned by conformance
tests against a model of a Spring node (see [Validation](#validation)). Qualify it on your
own logs before relying on it. [CONTRACT.md](CONTRACT.md) states exactly what a client can
rely on.

**Contents:**
[Install](#install) ·
[Quick start](#quick-start) ·
[How it works](#how-it-works) ·
[Inputs](#inputs) ·
[Operations](#operations) ·
[Command reference](#command-reference) ·
[Limits](#limits) ·
[Validation](#validation) ·
[Development](#development)

## Install

From the Hyperion Tools repository root:

```sh
cargo build --release --locked --package antelope-ship-archive
./target/release/antelope-ship-archive --help
# or
cargo install --path crates/ship-archive --locked
```

Tagged releases also ship a static Linux binary in the toolkit tarballs, with this README and
[CONTRACT.md](CONTRACT.md) under `docs/ship-archive/`, and a container image (see
[Docker](#docker)).

## Quick start

You copied one range of blocks off a node: the retained state-history files and the block-log
files for the same blocks, each with its index.

```text
/srv/archive/seg-0001/
├── state-history/
│   ├── trace_history-1000001-1001000.log        trace_history-1000001-1001000.index
│   ├── chain_state_history-1000001-1001000.log  chain_state_history-1000001-1001000.index
│   └── ...                                      (up to block 1003000)
└── blocks/
    ├── blocks-1000001-1001000.log               blocks-1000001-1001000.index
    └── ...
```

**1. Check it.** Read-only: the validation `serve` runs at startup, without serving.

```sh
cd /srv/archive/seg-0001
antelope-ship-archive check --dir state-history --dir blocks
```

```text
chain id      aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906
head and LIB  block 1003000 000f4df86ab0b36b…

trace         serving 1000001..1003001 of 1000001..1003001, in 3 files
              1000001..1001001       state-history/trace_history-1000001-1001000.log
              1001001..1002001       state-history/trace_history-1001001-1002000.log
              1002001..1003001       state-history/trace_history-1002001-1003000.log
              seams at 1001001, 1002001

chain_state   serving 1000001..1003001 of 1000001..1003001, in 3 files
              ...

finality      no files

blocks        serving 1000001..1003001 of 1000001..1003001, in 3 files
              ...

result: clean
```

Ranges are half-open: `1000001..1003001` is blocks 1,000,001 through 1,003,000. Run
`check --deep` once on every new copy: it reads every entry and block (see
[Check before serving](#check-before-serving)).

**2. Serve it** on a private address, with the HTTP endpoints for health checks and metrics:

```sh
antelope-ship-archive serve --dir state-history --dir blocks \
  --listen 10.0.0.5:8080 --http 10.0.0.5:9180
```

```text
INFO ship_archive: chain aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906
INFO ship_archive: trace        1000001..1001001  state-history/trace_history-1000001-1001000.log
...
INFO ship_archive: trace        serving 1000001..1003001
...
INFO ship_archive: head and LIB: block 1003000 000f4df86ab0b36b…
INFO ship_archive: listening on ws://10.0.0.5:8080 (at most 256 sessions)
INFO ship_archive: serving /health, /ready and /metrics on http://10.0.0.5:9180
INFO ship_archive: 16 io threads; each session builds up to 8 results ahead within a 1073741824-byte memory budget; at most 10804592640 bytes of results in all
```

**3. Point a SHiP-aware router or proxy at `ws://10.0.0.5:8080`.** Any SHiP client can
connect too: to it, the server is a node whose head never moves. `curl
http://10.0.0.5:9180/health` reports what is served:

```json
{"chain_id":"aca376f2…","findings":[],"last":{"id":"000f4df8…","num":1003000},
 "served":{"blocks":{"begin":1000001,"end":1003001},"chain_state":{"begin":1000001,"end":1003001},
 "finality":{"begin":0,"end":0},"trace":{"begin":1000001,"end":1003001}},"sessions":0,"version":"0.1.0"}
```

The outputs above are abbreviated (`…`), with illustrative numbers.

## How it works

### At startup

The server opens and validates everything once, before it listens. Any failure stops it with
a message naming the file and what to do.

1. **Discovery.** Every `--dir` (and with `--recursive`, its sub-directories) is scanned for
   the file names Spring writes (see [Layouts](#layouts)). Each log is paired with its index:
   a sidecar from `--index-dir` when one has its name, otherwise the index beside it.
2. **Strict open.** Every file is opened read-only with a reader that mirrors Spring's own
   open checks, without Spring's recovery: a log must end exactly at its last entry, and its
   index must hold one slot per block and point at that last entry. Nothing is ever repaired
   or rebuilt in memory, and there is no forward scan, so fork leftovers in a log (entries a
   node abandoned after a fork) are never read: every read goes through the index.
3. **Catalogs.** The files of each kind are ordered by the blocks they hold, which come from
   their contents, not their names. They must form one contiguous range: overlaps and holes
   are refused. Block logs must all belong to one chain.
4. **Served ranges.** For each state-history log, the blocks its files hold, cut to
   `--range` when given, and (by default) to the blocks the block logs hold, so that every
   served block has its `block`. The last servable block `L*` is one below the highest end.
5. **Block-id checks** (`--verify-ids seams`). At every file seam and at both edges of every
   served range, every log that knows a block's id must give the same one: the state-history
   entry headers, and the block log (block `n + 1`'s `previous`, or at the end of what it
   serves, the hash of block `n`'s header).
6. **Status.** head and LIB are both `{L*, id(L*)}`, the ranges are the served ones, and the
   chain id comes from the block logs (or `--chain-id`). Both status replies are encoded once.

### Per session

Each connection is a replica of a Spring 1.x SHiP session (`session.hpp`):

- The first message is the ABI, as one Text message: Spring's own, vendored byte for byte, or
  the `--abi` file. Every later message is Binary.
- Requests are decoded with fc's leniency. A `get_blocks` request replaces the current one,
  credit and cursor included, after Spring's `have_positions` rule moves its start back to
  the lowest listed block below it whose id this server does not know or knows differently.
  An ack adds to the credit and wraps at 32 bits, as Spring's plain addition does.
- Status requests are answered first and cost no credit. Then one block is sent while
  `credit > 0 && cursor <= L* && cursor < end`. Each block costs a credit and advances the
  cursor, including a block whose id is unknown (below the served range), which gets a result
  with head and LIB only, as from Spring.
- At the end of a request, or past `L*`, the session idles, open, and still answers status
  and new requests. It never sends anything unasked: no keep-alive results, no pings, and no
  Close except at shutdown.
- Every result is fully read and validated before its first frame: zlib framing, adler32 and
  sizes; every log naming the same block id; the block following `prev_block` and hashing to
  `this_block`. A read or integrity error drops the TCP connection without a Close frame, as
  Spring does on any error, so the client never receives part of a bad result. A payload large
  enough to be streamed is validated by a full counting pass first, so only an I/O error while
  it is read again can end a message midway.
- Results are framed as Spring frames them: the variant index, the result base, then for each
  payload its presence byte and length followed by the payload in chunks of at most 1 MiB,
  then an empty final frame. No frame exceeds 1 MiB.
- While one result is written, the next ones the client's credit allows are read ahead, up to
  `--prefetch`, within the shared `--memory-budget` (see [Memory](#memory)).

### What a result carries

| Field | Present when | From |
|---|---|---|
| head, `last_irreversible` | always | `{L*, id(L*)}` |
| `this_block` | block `n`'s id is known | the entry header of the first served log holding `n` (trace, then chain_state, then finality), else block `n + 1`'s `previous` in the served block log |
| `prev_block` | block `n − 1`'s id is known | the same, for `n − 1`; for the first served block, the block log's `previous` |
| `block` | `fetch_block`, `this_block` known, and the block log serves `n` | the stored `signed_block`, as Spring sends it |
| `traces`, `deltas`, `finality_data` | requested (`finality_data` only for a v1 request with `fetch_finality_data`), and `n` in that log's served range | the inflated entry payload, unchanged |

## Inputs

### Formats

| Log | Read | Refused |
|---|---|---|
| State history | Version 0, non-pruned: entries with an uncompressed-size preamble (Leap 4 and later, every Spring release, compressed or stored) and legacy entries (EOSIO 1.8 to Leap 3) | Pruned logs (`state-history-log-retain-blocks`); version-1 logs of EOSIO 2.1 and 2.2; logs from before nodeos 1.8 |
| Block log | Versions 1 to 3, non-pruned, flat or split | Pruned logs (`block-log-retain-blocks`): vacuum a copy first |

Every refusal names the file and the reason.

### Layouts

- **Names** are matched as Spring writes them: the head log `<stem>.log` and retained files
  `<stem>-<first>-<last>.log`, for the stems `trace_history`, `chain_state_history`,
  `finality_data_history` and `blocks`. Other files are ignored. Each log's index is
  `<stem>.index` (for example `trace_history-1000001-1001000.index`), beside the log or in an
  `--index-dir`.
- **Any number of directories**, flat or nested (`--recursive`): a node's `state-history`,
  `state-history-retained-dir`, `blocks` and `blocks-retained-dir`, or your own layout. A file
  reached twice (through a link, or a directory given twice) is read once.
- **Contents decide.** A retained name that disagrees with the blocks the file holds is a
  warning, and the contents are used. Empty files (a head log right after a rotation, a block
  log holding only its preamble) are skipped. A second identical copy of a file is dropped.
- **One contiguous range per server.** Any other overlap, and any hole, refuses startup,
  naming both files. `--range BEGIN-END` serves only `[BEGIN, END)`: every file is still
  opened and validated, but only the files holding a block of the range must line up. Serve
  each side of a hole with its own server.

### Block logs are needed too

SHiP results carry the `block` itself, and that comes from the **block log**, not from the
state-history logs. By default (`--blocks required`) the server needs the block log for the
same blocks and serves only what it covers: the state-history ranges are cut to the block-log
range, and startup warns about state history left out. The block log also gives the first
served block its `prev_block`, and it is what startup checks the state-history ids against.

**Plan disk space so that each archive server holds the block log for its whole range**,
lined up with its state-history files.

`--blocks none --chain-id <HEX64>` serves state history alone: results never carry `block`,
and startup logs a loud warning. Use it only where no client needs `block`: a SHiP-aware
router or proxy that stitches archives may count on every served block having one.

### Line up the strides

A node writes retained files at multiples of its strides: `state-history-stride` for state
history and `blocks-log-stride` for the block log. Make them equal, or make the state-history
stride a multiple of the block-log stride, so that every retained state-history file has
block-log files holding exactly its blocks. Otherwise copy every block-log file that overlaps
the range. Extra blocks are harmless: `--range` cuts what is served, and a block the block log
holds below the state-history range is answered as a node answers it, with `block` but no
traces or deltas.

### Copy only irreversible blocks

An archive's blocks never change, so they must already be final when they are copied. Copy a
state-history file only once the node's LIB has passed its last block. Spring rotates a file
at its stride boundary even while its last blocks are still reversible, and rotates it back if
a fork reaches into it, so a file copied too early can end on an abandoned fork. Never serve a
head log copied from a running node. Block logs only ever receive irreversible blocks.

Startup compares the ids at every seam and at both edges of the served ranges, and every block
sent must hash to its `this_block`, but only `check --deep` compares every block. A fork
entirely inside one file, away from the seams, is otherwise seen only when a client asks for
that block with `fetch_block`, and the connection then drops.

## Operations

### Check before serving

```sh
antelope-ship-archive check --dir state-history --dir blocks           # what serve validates
antelope-ship-archive check --dir state-history --dir blocks --deep    # read every block
antelope-ship-archive check --dir state-history --dir blocks --hash    # also hash every header
```

`check` takes the same store options as `serve` and prints the coverage per log, the files,
the seams, the chain id, every finding, and the result; `--json` prints one JSON document
(`ok`, `coverage`, `findings`, `warnings`, `errors`, `more_errors`, `deep`) instead. The
first 100 errors are listed, and the rest counted.

- Without `--deep` it opens and validates the logs as `serve` does, and sizes the served
  entries from their headers (a legacy entry that might be large is inflated once).
- `--deep` reads every served block: each entry's header, trailer and whole zlib stream, the
  block log's index, trailers, sizes and `previous` links, and it checks that every log agrees
  on every block id. It logs progress every 100,000 blocks.
- `--hash` (implies `--deep`) also recomputes every block's id from its header.
- An entry inflating to more than `--warn-entry-bytes` (default 256 MiB) is a warning: a
  SHiP-aware router or proxy with a 256 MiB message limit cannot relay it. An entry of 4 GiB
  or more is an error: SHiP clients read its length as a 32-bit varuint. Exclude either with
  `--range`.
- A block larger than 20 MiB (fc's limit for a result's `block`) is an error in `--deep`: a
  request for it with `fetch_block` drops the connection.

Exit status: 0 when clean, 1 on errors, and with `--strict` 2 on warnings. Notes (an empty
file skipped, a duplicate dropped) are not warnings.

### Missing or wrong indexes

A state-history log whose index is missing or does not match it refuses startup, with an
error (`index for …` or `index mismatch for …`) that ends in
``run `antelope-ship-archive index` ``. Rebuild the index into a sidecar directory, never
beside the log:

```sh
antelope-ship-archive index --dir state-history --out /srv/archive/seg-0001-index
antelope-ship-archive serve --dir state-history --dir blocks \
  --index-dir /srv/archive/seg-0001-index ...
```

```text
state-history/trace_history-1001001-1002000.log (blocks 1001001..1002001) has no index beside it: wrote /srv/archive/seg-0001-index/trace_history-1001001-1002000.index
1 of 1 indexes written to /srv/archive/seg-0001-index
```

`index` rebuilds each index as Spring does: it walks the log backward from its last entry
through the position trailers, skips entries for blocks at or past the end (fork leftovers),
keeps the first entry seen from the end for each block, and stops at the log's first block.
Every block must be found. It writes an index only where the one beside the log is missing or
differs (or where `--out` holds a different one), checks it with the strict reader, then
renames it into place. An `--out` that holds one of the logs is refused, and so are two logs
that would share a sidecar name (index them into different `--out` directories). An index
that opens but points at fork leftovers is caught by `check --deep`; `index` then writes the
right one to `--out`, and `serve --index-dir` uses it.

Block-log indexes are not rebuilt here (Spring's `spring-util block-log make-index` rebuilds
one). An index in an `--index-dir` under a block log's index name is used too.

### The ABI

Every session starts with Spring's SHiP ABI text, vendored byte for byte. To send the text your
live nodes send instead (another release or fork), fetch it once and serve it with `--abi`:

```sh
antelope-ship-archive abi --from ws://10.0.0.20:8080 --out ship-abi.json
antelope-ship-archive serve --abi ship-abi.json ...
```

`abi` connects, reads the first message, checks that it is JSON text, and writes it unchanged
(to standard output without `--out`). Clients behind a SHiP-aware router or proxy then decode
archived and live results with the same text.

### Health and metrics

`--http ADDR` serves, from one thread beside the SHiP listener:

| Endpoint | Response |
|---|---|
| `/health` | 200 and JSON: `version`, `chain_id`, `served` (`trace`, `chain_state`, `finality`, `blocks`, each `{begin, end}`), `last` (`{num, id}`), `sessions`, `findings` |
| `/ready` | 200 while the server accepts sessions (the logs are validated and the listener is bound), 503 once it shuts down |
| `/metrics` | Prometheus text exposition |

Anything else is 404, a method other than GET or HEAD is 405, and query strings are ignored.
The metrics, all prefixed `antelope_ship_archive_`:

| Series | Kind | Meaning |
|---|---|---|
| `sessions` | gauge | Open SHiP sessions |
| `sessions_rejected_total` | counter | Upgrades refused with HTTP 503 at `--max-sessions` |
| `status_requests_total{version}` | counter | Status requests, `v0` or `v1` |
| `requests_total{version}` | counter | `get_blocks` requests, `v0` or `v1` |
| `results_sent_total` | counter | Block results sent |
| `bytes_sent_total` | counter | Message bytes sent: results and status replies |
| `read_errors_total{log}` | counter | Read or integrity errors that dropped a session, by `log`: `trace`, `chain_state`, `finality`, `blocks` |
| `read_seconds_sum`, `read_seconds_count` | summary | Time spent reading and inflating logs, and the number of reads |
| `memory_budget_bytes{state}` | gauge | The `--memory-budget`, `used` or `free` |
| `served_begin_block{log}`, `served_end_block{log}` | gauge | The served range `[begin, end)` of each log, `0` and `0` when not served |

### Logs

Logs go to standard error, filtered with `RUST_LOG` (default `info`), as text or with
`--log-format json` one JSON object per line.

- At startup: the chain id, every file of every log with its blocks, what is served of each,
  the findings, head and LIB, the listener, and the memory bound.
- One INFO line per session when it closes, with the peer, the status, block and ack requests,
  the results and bytes sent, the duration, and why it ended: `closed by the client`,
  `disconnected without a close`, `read failed`, `undecodable request, dropped`, `more than
  1024 status requests queued, dropped`, `send failed`, `log read error, dropped`, or `server
  shutdown`.
- One WARN line per read or integrity error, with the peer, the block, and the cause, which
  names the file and the offset.
- One INFO line per upgrade refused with 503.

### Shutdown

On SIGINT (Ctrl-C) or SIGTERM the server stops accepting, `/ready` turns 503, and each session
gets WebSocket Close 1001 after the message it is writing. Sessions get 5 s to finish; the
rest are then dropped.

### Docker

`crates/ship-archive/Dockerfile` builds a static binary on Alpine, running as uid 10001. Build
it from the repository root (`docker build -f crates/ship-archive/Dockerfile .`); tagged
releases publish it as `ghcr.io/eosrio/antelope-ship-archive`. Mount the logs read-only at
`/data`, as a directory uid 10001 can read:

```sh
docker run -p 8080:8080 -p 9180:9180 -v /srv/archive/seg-0001:/data:ro \
  ghcr.io/eosrio/antelope-ship-archive
```

The default command is `serve --dir /data --recursive --listen 0.0.0.0:8080 --http
0.0.0.0:9180`. To change options, give the whole command after the image name (for example
`serve --dir /data --recursive --range 2-50000001 --listen 0.0.0.0:8080 --http
0.0.0.0:9180`). The image's `HEALTHCHECK` polls `http://127.0.0.1:9180/ready`, so keep
`--http` on port 9180. The offline commands run the same way: `... check --dir /data
--recursive --deep`.

### Security

- The default listen address is loopback (`127.0.0.1:8080`).
- There is **no authentication and no TLS**. Run the server on a private network, behind a
  SHiP-aware router or proxy, and never expose it to the internet.
- Every log and index is opened read-only, and nothing is ever written beside the logs. Only
  `index --out` and `abi --out` write files, and only where they are told to.

### Behind a SHiP-aware router or proxy

- Status reports the served ranges, head = LIB = `L*`, and the chain id. They never change
  while the server runs.
- At its limit the server refuses the upgrade with HTTP 503 and `Retry-After: 1`; it never
  accepts a session and then drops it. Use another replica, or retry.
- A TCP connection dropped without a Close frame means a bad request or a read or integrity
  error on this server's files: fail over to a replica. Close 1001 means it is shutting down.
- The server never closes a session on its own and has no idle timeout. TCP keepalive (after
  60 s idle, then every 10 s, 5 probes) frees the slot of a peer that vanished without
  closing.
- Run replicas: servers over copies of the same blocks, with the same `--abi`, send identical
  bytes.

## Command reference

| Command | Purpose | Writes |
|---|---|---|
| `serve` | Serve the logs of one contiguous range | Nothing |
| `check` | Validate the logs as `serve` would, and report | Nothing |
| `index` | Rebuild missing or wrong state-history indexes | Sidecar indexes in `--out` |
| `abi` | Print the ABI text a SHiP node sends first | `--out`, when given |

**Store options** (`serve` and `check`):

| Option | Default | Meaning |
|---|---|---|
| `--dir DIR` | required | A directory of state-history and block logs; repeatable |
| `--recursive` | off | Also scan the sub-directories of every `--dir` |
| `--index-dir DIR` | none | Sidecar indexes, used instead of the index beside a log; repeatable, the first holding a name wins |
| `--range BEGIN-END` | everything | Serve only blocks `BEGIN` to `END − 1` |
| `--blocks required\|none` | `required` | Whether results carry `block` from the block logs (`none` needs `--chain-id`) |
| `--chain-id HEX64` | from the block logs | Required with `--blocks none`; must match the block logs otherwise |
| `--verify-ids seams\|off` | `seams` | Cross-check block ids at seams and range edges before serving |
| `--max-open-files N` | 256 | Log files kept open at once, least recently used closed first |

**`serve` options:**

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | `127.0.0.1:8080` | The SHiP WebSocket address |
| `--abi FILE` | Spring's ABI | Send this ABI text (JSON) first instead |
| `--max-sessions N` | 256 | Upgrades beyond this many open sessions get HTTP 503 |
| `--max-request-bytes BYTES` | 1048576 | The largest client message; a larger one drops the connection |
| `--worker-threads N` | available parallelism | Runtime worker threads |
| `--io-threads N` | `min(16, 2 × cores)` | File reads and inflations running at once |
| `--prefetch N` | 8 | Results each session builds ahead, at most (0: none) |
| `--memory-budget BYTES` | 1073741824 | Bytes all results built ahead may hold together |
| `--stream-threshold BYTES` | 4194304 | Payloads inflating to more are streamed instead of held in memory |
| `--http ADDR` | off | Serve `/health`, `/ready` and `/metrics` |
| `--log-format text\|json` | `text` | Log line format |

**`check` options:** `--deep`, `--hash` (implies `--deep`), `--warn-entry-bytes N` (default
268435456), `--strict` (exit 2 on warnings), `--json`.

**`index` options:** `--dir DIR` (repeatable, required), `--recursive`, `--out DIR`
(required; created if missing).

**`abi` options:** `--from ws://HOST:PORT` (required), `--out FILE`.

Every command takes `--help`.

## Limits

### Memory

A server holds two kinds of result buffers:

- **Results built ahead** of each session's writer, at most `--prefetch` per session and never
  beyond the client's credit, all sharing `--memory-budget`. A build reserves the most its
  result can hold (its block, its payloads up to the stream threshold, and 1.5 MiB of read
  scratch) and keeps what the result holds once read. A build that does not fit is skipped and
  built when it is sent instead: building ahead never waits for memory, so a budget below
  about 1.6 MiB builds nothing ahead.
- **The result each session is sending**, built on demand outside the budget: at most three
  payloads of up to `--stream-threshold` bytes, one block of up to 20 MiB and the result base,
  plus about 4.25 MiB of streaming and write buffers. A session never waits for memory another
  session holds.

So the result buffers stay within

```text
memory_budget + max_sessions × (3 × stream_threshold + 20 MiB + 4.25 MiB + 256 B)
```

which the server logs at startup (`at most N bytes of results in all`):

| Settings | Bound |
|---|---|
| Defaults (`--memory-budget` 1 GiB, `--max-sessions 256`, `--stream-threshold` 4 MiB) | 10,804,592,640 bytes (about 10.1 GiB) |
| `--max-sessions 32` | 2,290,098,176 bytes (about 2.1 GiB) |
| `--max-sessions 32 --stream-threshold 1048576 --memory-budget 268435456` | 1,182,801,920 bytes (about 1.1 GiB) |

The bound assumes every session is sending a maximal result at once; real loads stay far
below it. It covers result buffers, not the runtime, the open files, or the operating system's
page cache, which caches the logs. Size it with `--max-sessions` first. A session that ends
while its next result is being read lets that read finish on its own, so sessions that come
and go quickly can exceed the bound briefly.

### Large entries and blocks

- A payload inflating to more than `--stream-threshold` is streamed in 1 MiB frames. It is
  inflated twice: once to validate it before the first frame, and once while it is sent. A
  legacy entry, which does not record its size, is first inflated up to the threshold to
  find out that it is larger.
- Entries above 256 MiB (for example the full-state delta a node writes as the first
  chain_state entry of a log started from a snapshot) are served, but a SHiP-aware router or
  proxy with a 256 MiB message limit cannot relay them. `check` warns about them. Raise the
  router's limit or exclude them with `--range`.
- Entries of 4 GiB or more are served too, but SHiP clients, which read the length as a
  32-bit varuint, cannot decode them; `check` reports them as errors.
- A block above 20 MiB cannot be sent in a result: a request for it with `fetch_block` drops
  the connection before any frame.

### Other limits

- **SHiP only.** The block logs feed results' `block`; there is no `/v1/chain` or other HTTP
  API over them.
- **One contiguous range per server**, never extended while it runs: restart it to serve new
  files.
- **No authentication, no TLS**, and `abi --from` speaks `ws://` only.
- **Payloads are forwarded as stored**, inflated but never re-serialized. That history written
  by EOSIO 2.0 or Leap 3 decodes under Spring's ABI is not verified; `--abi` serves the text a
  node of that era sends.
- **Limits per session:** requests up to `--max-request-bytes`, at most 1024 queued status
  requests (more drop the connection), and 10 s to complete the WebSocket upgrade.
- CI builds and tests it on Linux and Windows.

## Validation

`cargo test --locked --package antelope-ship-archive` runs the server in-process on loopback
over logs written by the core's fixture writers (one synthetic chain with consistent ids and
`previous` links; Spring stored, Leap compressed and legacy entry encodings; retained, split
and empty files):

- `tests/conformance.rs` compares every status and result byte for byte with an independent
  model of a Spring node holding the same logs: the ABI message, status v0 and v1, result
  variants, credit replacement and acks, `have_positions`, idling at the end, results below
  and past the range, all 32 combinations of request version and fetch flags, ids across file
  seams, `--blocks none`, streamed payloads and 1 MiB frames, corrupt inline and streamed
  entries (dropped before their first frame), 503 at the session limit, Close 1001 at
  shutdown, malformed requests, a status flood, a new request discarding results built ahead,
  startup refusals (ids that disagree at a seam or at the end of the served range), and a
  block that does not hash to its id dropping the connection.
- `tests/hardening.rs` checks every endpoint and metric, that results are the same bytes
  however much is built ahead (from `--prefetch 0` and a 1-byte budget to one io thread), and
  that nothing is built beyond the client's credit.
- `tests/memory.rs` runs 64 sessions that request large results and never read them, and
  requires the whole process's allocations to stay within the memory bound; the same load
  without an effective budget exceeds it.
- `tests/offline.rs` covers `check` (exit codes, large entries, `--deep`, `--hash`, through
  the binary), `index` (an index pointing at fork leftovers, rebuilt and then served), and
  `abi`.

The core's strict readers are tested on their own with fault injection (torn tails, wrong
indexes, bad checksums, pruned and unsupported formats) and on real EOS and WAX entries and a
real EOS block from `crates/core/testdata/`.

**Not covered yet:** end-to-end runs over real Spring logs and a byte-for-byte comparison with
a running node. Until then, run `check --deep` on your copies and compare some results with a
node holding the same blocks.

## Development

```sh
cargo fmt --package antelope-ship-archive --check
cargo clippy --locked --package antelope-ship-archive --all-targets -- -D warnings
cargo test --locked --package antelope-ship-archive
cargo test --locked --package hyperion-ship --features archive,testkit   # the core readers
```

`.github/workflows/ship-archive.yml` runs these on Ubuntu and Windows. The crate is not
published to crates.io.

The readers, the catalog, the protocol codec and the vendored ABI live in the core
(`hyperion-ship`, feature `archive`); `scripts/extract_ship_abi.py` re-extracts the ABI from a
Spring checkout. This crate holds the server: `store.rs` (validation, served ranges, building
results), `session.rs` (the Spring session), `fetch.rs` (building ahead, the budget, the io
threads), `frames.rs`, `http.rs`, `metrics.rs`, `check.rs` and `cli.rs`.
