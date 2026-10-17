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

With `--api` it also answers nodeos's `/v1/chain` block endpoints (`get_block_info`,
`get_block_header`, `get_raw_block`) from its block logs, as a Spring node holding those blocks
answers them, so that an HTTP router or proxy in front of nodeos-compatible APIs can send
requests for old blocks there too (see [Chain API](#chain-api)).

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
[Chain API](#chain-api) ·
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
{"chain_id":"aca376f2…","findings":[],"first":{"num":1000001,"prev_block":{"id":"000f4240…","num":1000000}},
 "last":{"id":"000f4df8…","num":1003000},
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
   serves, the hash of block `n`'s header). The block just below each served range, which
   the range's first block names as its `prev_block`, must have one id in every open file
   holding it, served or not. With `--verify-ids off` none of this is checked, and that block
   is read only where its id comes from: a file that cannot be read there is skipped with a
   warning instead of refusing startup.
6. **Status.** head and LIB are both `{L*, id(L*)}`, the ranges are the served ones, and the
   chain id comes from the block logs (or `--chain-id`). Both status replies are encoded once.

### Per session

Each connection is a replica of a Spring 1.x SHiP session (`session.hpp`):

- The first message is the ABI, as one Text message: Spring's own, vendored byte for byte, or
  the `--abi` file. Every later message is Binary.
- Requests are decoded with fc's leniency. A `get_blocks` request replaces the current one,
  credit and cursor included, after Spring's `have_positions` rule moves its start back to
  the lowest listed block below it whose id this server does not know or knows differently.
  The block before the first served one counts as known when a file names it, as the first
  result names it as `prev_block`.
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
| `prev_block` | block `n − 1`'s id is known (never for block 1: there is no block 0) | the same, for `n − 1`; for the first served block, any open file naming `n − 1`, as a node fills it from any log it has open: the block log's `previous` of `n`, or a state-history file holding `n − 1` below `--range` |
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
  opened and validated, but only the files holding a block of the range must line up. The
  state-history file holding block `BEGIN − 1` is kept too when it lines up: it names the
  first block's `prev_block`, so servers split by `--range` over one directory send at each
  seam what one server sends. Serve each side of a hole with its own server.

### Block logs are needed too

SHiP results carry the `block` itself, and that comes from the **block log**, not from the
state-history logs. By default (`--blocks required`) the server needs the block log for the
same blocks and serves only what it covers: the state-history ranges are cut to the block-log
range, and startup warns about state history left out. The block log also gives the first
served block its `prev_block`, and it is what startup checks the state-history ids against.
Without it, only a state-history file holding the block before the first served one names
that block, and startup warns when none does (see
[The block before the range](#the-block-before-the-range)). The [chain API](#chain-api)
answers from the block log too.

**Plan disk space so that each archive server holds the block log for its whole range**,
lined up with its state-history files.

`--blocks none --chain-id <HEX64>` serves state history alone: results never carry `block`,
startup logs a loud warning, and `--api` is refused. Use it only where no client needs
`block`: a SHiP-aware router or proxy that stitches archives may count on every served block
having one.

### The block before the range

The first served block names the block before it as its `prev_block`, as one server over all
the logs sends it, and as a Spring node does. So a client streaming across servers split by
`--range`, or across copies cut to their range, gets at each seam the bytes one server would
send, and can check that the second server continues the first. That block is in no served
log, so startup looks up its id once, in every open file that names it, served or not, in the
order Spring reads its logs:

1. the entry header of each state-history file holding it (trace, then chain_state, then
   finality). The catalogs keep the file holding the block just below `--range`, when it lines
   up with the rest, even if it holds no served block;
2. the block log's `previous` of the first served block;
3. the hash of that block's own header, where a block log holds it.

With `--blocks required` the served block log always names it (2). With `--verify-ids seams`
(the default) every file naming it must give the same id, or startup is refused (`block ids
disagree at block …`). With `--verify-ids off` only what gives the id is read: nothing below
the range when the served block log names it, else the files in the order above up to the
first that can be read. A file that cannot be read is then skipped with a startup warning
(`--verify-ids off: a file naming block …, the prev_block of block …, cannot be read and is
skipped: …`). `have_positions` count that id as known, as they count the served ones.

When no file names it (`--blocks none` over files that begin where the range begins), the
first result has no `prev_block`, and:

- startup logs one warning, for example
  ```text
  WARN ship_archive: block 1000001, the first served, gets no prev_block: no open log names block 1000000, so a client cannot check that block 1000001 follows the blocks before it; serve the block log holding block 1000001, or the state-history files holding block 1000000
  ```
  (`a file naming block 1000000 cannot be read` and `repair it` when `--verify-ids off`
  skipped the file that names it);
- `check` lists the same warning, so `check --strict` exits 2;
- `/health` reports `"first":{"num":1000001,"prev_block":null}`.

Without block logs, a history from genesis is the exception: its first block, 2, follows
block 1, the genesis block, which is in no state-history log and which no server sends. Block
2 then gets no `prev_block`, and nothing warns. A copy without block logs that begins at a
snapshot does warn: nothing in its files tells it from a copy cut to its range.

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
the seams, the chain id, the first block and its `prev_block` (a warning when no file names
the block before it), every finding, and the result; `--json` prints one JSON document
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
| `/health` | 200 and JSON: `version`, `chain_id`, `served` (`trace`, `chain_state`, `finality`, `blocks`, each `{begin, end}`), `first` (`{num, prev_block}`: the first served block and the `prev_block` its result carries, `{num, id}` or `null` for block 1 or when no file names the block before it), `last` (`{num, id}`), `sessions`, `findings`, and with `--api` `api` (`{listen, get_block, in_flight, max_in_flight}`, see [Chain API](#chain-api)) |
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
| `read_seconds_sum`, `read_seconds_count` | summary | Time spent reading and inflating logs on the io threads, and the number of reads; chain API block reads included |
| `memory_budget_bytes{state}` | gauge | The `--memory-budget`, `used` or `free`; chain API reservations included |
| `served_begin_block{log}`, `served_end_block{log}` | gauge | The served range `[begin, end)` of each log, `0` and `0` when not served |
| `api_requests_total{endpoint,code}` | counter | Chain API answers, by `endpoint` (`get_block_info`, `get_block_header`, `get_raw_block`, `get_block`, `get_supported_apis`, `describe`, or `other`: any other target, `OPTIONS`, an illegal target) and HTTP status; a pair appears once counted |
| `api_request_seconds_sum{endpoint}`, `api_request_seconds_count{endpoint}` | summary | Time from a chain API request's headers to its answer |
| `api_in_flight` | gauge | Chain API block requests holding a slot (`--api-max-in-flight`) |
| `api_rejected_total{reason}` | counter | Chain API requests and connections refused, or closed before their answer was written: `in_flight` (no slot within `--api-queue-ms`), `memory` (the budget is full), `body_too_large`, `timeout` (no request head, no whole body, or none of the answer taken within 10 s; an idle kept-alive connection closed after its requests is not counted), `connections` (over `--api-max-connections`) |
| `api_failures_total{reason}` | counter | Chain API requests answered 503 for a failure on the server's side: `read`, `integrity`, `panic` |
| `api_response_bytes_total` | counter | Chain API response body bytes |

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
- With `--api`: at startup, the chain API's address, the blocks it answers and its limits; one
  WARN line per chain API request whose block cannot be read or fails validation, or whose job
  panicked, with the peer, the endpoint, the block and the cause; and at DEBUG, one line per
  request with the peer, the endpoint, the block, the status, the body bytes and the time in
  µs. Request bodies are never logged.

### Shutdown

On SIGINT (Ctrl-C) or SIGTERM the server stops accepting, `/ready` turns 503, and each session
gets WebSocket Close 1001 after the message it is writing. The chain API stops accepting at
the same time: each of its connections finishes the request it is answering, then closes, and
idle ones close at once. Sessions and chain API connections get 5 s to finish; the rest are
then dropped.

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

The chain API is off in the default command. The image declares port 8888 for it: add `--api
0.0.0.0:8888` to the command and publish the port on a private address only:

```sh
docker run -p 10.0.0.5:8080:8080 -p 10.0.0.5:9180:9180 -p 10.0.0.5:8888:8888 \
  -v /srv/archive/seg-0001:/data:ro ghcr.io/eosrio/antelope-ship-archive \
  serve --dir /data --recursive --listen 0.0.0.0:8080 --http 0.0.0.0:9180 --api 0.0.0.0:8888
```

### Security

- The default listen address is loopback (`127.0.0.1:8080`), and the chain API is off unless
  `--api` is given.
- There is **no authentication and no TLS**, on the SHiP listener, `--http` and `--api` alike.
  Run the server on a private network, behind a SHiP-aware router or proxy (and for `--api`,
  an HTTP router or proxy), and never expose it to the internet.
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

## Chain API

`--api ADDR` serves nodeos's `/v1/chain` block endpoints from the served block logs, on a
listener of its own, as a Spring 1.1.0 to 1.2.2 node holding those blocks answers them. It is
off by default. It is refused with `--blocks none`, and on the `--listen` or `--http` address.

```sh
antelope-ship-archive serve --dir state-history --dir blocks \
  --listen 10.0.0.5:8080 --http 10.0.0.5:9180 --api 10.0.0.5:8888
```

```text
INFO ship_archive: serving the chain API on http://10.0.0.5:8888 for blocks 1000001..1003001: at most 256 connections and 4 requests at once, each waiting up to 250ms for its turn; bodies up to 2097152 bytes
```

```sh
curl -X POST http://10.0.0.5:8888/v1/chain/get_block_info -d '{"block_num":1003000}'
```

The chain API has **no TLS, no authentication, no CORS and no `Host` check**. Archives are
private and operator-run: bind `--api` to a private address and publish it only through an
HTTP router or proxy in front of nodeos-compatible APIs, which can send each request for an
old block to the server holding it, and everything else to live nodes. Never expose it to the
internet. [CONTRACT.md](CONTRACT.md#chain-api) states what such a router can rely on.

### Endpoints

| Target | Answer |
|---|---|
| `/v1/chain/get_block_info` | The block's info, as nodeos renders it |
| `/v1/chain/get_block_header` | The signed block header, and with `include_extensions` the block extensions |
| `/v1/chain/get_raw_block` | The whole `signed_block`, transactions as stored |
| `/v1/node/get_supported_apis` | `{"apis":[...]}`: the three paths above, sorted |
| `/v1/archive/describe` | What this server serves, for a router (below); not a nodeos endpoint |
| Anything else | nodeos's `404 Unknown Endpoint` |

Anything else includes `get_info`, the state endpoints, push, and `get_block`, which decodes
action data with the contracts' ABIs and is left to live nodes.

A block is found when its number is in the served block-log range (cut to `--range`:
`served.blocks` in `/health`, `blocks` in `describe`), and, asked by id (`get_block_header`
and `get_raw_block`), when the stored block hashes to that id. Any other block gets nodeos's `400 Unknown Block`. The block-log range can
be wider than the state-history ranges: a block log from block 1 answers for block 1, which no
state-history log holds.

Requests are read as nodeos reads them: its empty-body rule, fc's legacy JSON parser (missing
and repeated commas, `\X` escapes, the first of duplicate keys, bytes after the object
ignored) and its conversions. A block number is cut to 32 bits (`"4294967297"` is block 1),
and `block_num_or_id` is a number, or an id of an even number of hex digits, at most 64,
whose first four bytes give the block number. A fraction converts as Spring's x86-64 release build converts it. The block
bodies, and the errors nodeos gives for these endpoints, are byte for byte what a Spring 1.1.0
to 1.2.2 node answers with `verbose-http-errors` off.

### HTTP

- HTTP/1.1, and HTTP/1.0; no HTTP/2, no TLS.
- The request-target is checked first: one that is not origin-form (`OPTIONS *`,
  `http://host/...`) or holds `..` gets `400 Illegal request-target`.
- `OPTIONS`, on any path, gets `200 {}`.
- The handler is the one whose path is the whole target, query string included:
  `/v1/chain/get_raw_block?x=1` gets the 404.
- Any method reaches the handler, with its body.
- Every answer the server writes has `Content-Type: application/json` and a
  `Content-Length`, title-case header names in nodeos's order,
  `Server: antelope-ship-archive/<version>`, and no `Date`.
- Connections are kept alive as the client asks: HTTP/1.1 unless it says `close`, HTTP/1.0 when
  it says `keep-alive`.

### Limits and errors

| When | Answer |
|---|---|
| The body is larger than `--api-max-body-bytes` (default 2 MiB, nodeos's `max-body-size`), declared or as it arrives | `413 Payload Too Large`, then the connection is closed |
| The headers take more than 10 s to arrive, or the body more than 10 s after them | The connection is closed unanswered |
| A kept-alive connection stays idle 10 s, or a client takes none of its answer for 10 s | The connection is closed |
| `--api-max-connections` connections are open | One more is answered `503 Busy` (`Too many connections: N`) before its request is read, and closed |
| No request slot frees within `--api-queue-ms` | `503 Busy` (`Too many requests in flight: N`) |
| The memory budget cannot hold the request | `503 Busy` (`Too many bytes in flight: N`, the bytes of the budget in use); for a request body, the connection is closed too |
| The block cannot be read | `503 Busy` (`Block read failed`) and a WARN line |
| The block is larger than 20 MiB, does not decode, holds another block number than it is stored under, or does not render | `503 Busy` (`Block data failed validation`) and a WARN line |
| The request's job panics | `503 Busy` (`Internal error`) and a WARN line |

A 503 has the body nodeos sends when it is overloaded, with the reason as `what`. A 413 has a
body of the same shape, which nodeos never sends:

```text
{"code":503,"message":"Busy","error":{"code":503,"name":"Busy","what":"<why>","details":[]}}
{"code":413,"message":"Payload Too Large","error":{"code":413,"name":"Payload Too Large","what":"Request body exceeds <N> bytes","details":[]}}
```

A 503 is never final: the request can be sent again, here or elsewhere. The only 500 answers
are nodeos's own `Invalid block ID`, for a `block_num_or_id` that is neither a number nor
even-length hex, or is empty or longer than 64 characters.

### Requests, io threads and memory

- A request that does not parse, or names a block number outside the served block-log range,
  is answered at once, with no file read.
- Any other block request takes one of `--api-max-in-flight` slots (default a quarter of
  `--io-threads`, at least 2), waiting at most `--api-queue-ms` (default 250) for one. It then
  reads and renders its block on one of the io threads the SHiP sessions read on.
- What it holds is reserved first from `--memory-budget`, the budget SHiP sessions build ahead
  in, and never waits for it: the block, and room for the body, 64 KiB for `get_block_info`
  and `get_block_header`, and three times the block more for `get_raw_block` and
  `get_block_header` with `include_extensions`. A body that outgrows that room (a block of many
  id-only receipts) is rendered once more under 17 times the block plus 64 KiB, which no block
  outgrows.
- Once the body is rendered, the block is dropped, and the reservation shrinks to the body,
  which keeps it until it is written out. A client that takes none of its answer for 10 s is
  cut off, and the reservation is given back.
- A request body holds its first 16 KiB outside the budget, and reserves the rest as it
  arrives.

So besides `--memory-budget`, each of at most `--api-max-connections` connections holds only
hyper's buffers (a request head is at most 16 KiB), the response head and at most 16 KiB of
request body. The bound the server logs at startup (see [Memory](#memory)) leaves these
connection buffers out.

Block requests share the io threads and the memory budget with SHiP sessions, and are bounded
by the slots and by reservations that never wait: a SHiP session never waits for memory a
chain API request holds, as it never waits for memory another session holds.

### `/v1/archive/describe`

What an HTTP router or proxy needs to route block requests here, answered from memory, with no
file read, for any method, whatever the body:

```json
{"server":"antelope-ship-archive/0.1.0","chain_id":"aca376f2…",
 "last":{"num":1003000,"id":"000f4df8…"},
 "blocks":{"begin":1000001,"end":1003001},
 "apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","/v1/chain/get_raw_block"],
 "get_block":"off","max_in_flight":4,"in_flight":0}
```

- `chain_id` and `last` are the SHiP status's chain id and head (= LIB, `L*`).
- `blocks` is the served block-log range, `[begin, end)`: exactly the blocks the endpoints
  answer.
- `apis` lists the block endpoints served, sorted. `get_block` is always `"off"`.
- `max_in_flight` is `--api-max-in-flight`, and `in_flight` the slots taken now.

`get_supported_apis` lists neither `describe` nor itself, as nodeos never lists itself.
`/health` reports the listener as
`"api":{"get_block":"off","in_flight":0,"listen":"10.0.0.5:8888","max_in_flight":4}`.

### Differences from nodeos

- **No `Host` check.** A request without a `Host`, or with an empty one, is answered as the
  same request with one; nodeos refuses it with `400 Disallowed HTTP HOST header in the
  request`, even with `http-validate-host = false`. Any other `Host` is accepted, as with
  `http-validate-host = false`.
- **Oversized bodies** get `413` and the connection is closed; nodeos closes the connection
  without an answer.
- **A block that cannot be read or fails validation** gets `503 Busy`, nodeos's overload
  answer, so that a client or a router asks elsewhere; a node with a damaged block log would
  answer otherwise.
- **Several `Connection` lines** in an HTTP/1.1 request: the connection is closed once any of
  them says `close`, and the answer says `Connection: close`; nodeos reads the first line alone
  and may keep the connection.
- **A `#fragment`** in the request-target is dropped: `/v1/chain/get_raw_block#x` is answered
  as `/v1/chain/get_raw_block`, where nodeos answers 404, and a `..` after the `#` escapes the
  illegal-target check.
- **A target byte outside the URI grammar** (`<`, `>`, a backquote, a control or non-ASCII
  byte) gets hyper's own `400 Bad Request`, with an empty body and no `Content-Type` or
  `Server`, and the connection is closed; nodeos answers its JSON 404 or 400.
- **HEAD** is answered with the head alone; nodeos sends the body too.
- **`Expect: 100-continue`**: the interim `100 Continue` carries no `Server` header.
- **`Server`** is `antelope-ship-archive/<version>`.
- **Error `line_number`s** are Spring 1.1.0 to 1.2.2's; other releases put these errors on
  other source lines. `verbose-http-errors` is not mirrored.

Clients send neither a fragment nor a raw byte outside the URI grammar: a fragment stays in the
client, and such bytes must be percent-encoded in a target.

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

**`serve` chain API options** (see [Chain API](#chain-api); the others mean nothing without
`--api`):

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | off | Serve the `/v1/chain` block endpoints at this address; it must differ from `--listen` and `--http`, and is refused with `--blocks none` |
| `--api-max-in-flight N` | a quarter of `--io-threads`, at least 2 | Block requests read and rendered at once, each on an io thread (at least 1) |
| `--api-queue-ms MS` | 250 | How long a block request waits for a slot before `503 Busy` |
| `--api-max-body-bytes BYTES` | 2097152 | The largest request body; a larger one gets 413 and the connection is closed (at least 64) |
| `--api-max-connections N` | 256 | Connections open at once; one more is answered `503 Busy` and closed (at least 1) |

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

The chain API (`--api`) reserves what its requests hold from `--memory-budget`, so it adds to
the bound only the buffers of its connections, at most `--api-max-connections` of them (see
[Requests, io threads and memory](#requests-io-threads-and-memory)).

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

- **Block endpoints only.** Besides SHiP, the block logs serve `get_block_info`,
  `get_block_header` and `get_raw_block` (`--api`); every other nodeos endpoint, `get_block`
  and `get_info` included, gets 404.
- **One contiguous range per server**, never extended while it runs: restart it to serve new
  files.
- **No authentication, no TLS**, and `abi --from` speaks `ws://` only.
- **Chain API limits:** request bodies up to `--api-max-body-bytes`, request heads up to
  16 KiB (hyper refuses a larger one before the server sees it), and 10 s for the headers,
  then for the body, for an idle kept-alive connection, and for a client taking none of its
  answer. The 10 s limits are not configurable.
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
- `tests/seams.rs` splits one history across servers by `--range` (inside files, on a
  block-log seam, and where every file ends) and across copies cut to their range, with and
  without block logs, and requires every result of each server to equal one server's over
  all the logs but for head and LIB, the first block's `prev_block` included. It also covers
  the copy without block logs whose first block nothing names (the startup warning, `check`
  and `/health`), files that disagree on the block before a range, `--verify-ids off` over
  files that cannot be read there, a `have_positions` entry naming that block, and a history
  from block 2.
- `tests/real_slices.rs` serves the real EOS and WAX entries of `crates/core/testdata/`, each
  as a one-entry log, inline and streamed, as v0 and v1 results, and the real EOS block under
  the id its header hashes to, all byte for byte. Its ignored tests read a copy of a node's
  logs from outside the repository: byte-for-byte runs against the strict readers, a
  full-speed run, `--range` seams, the full-state first entry and `check --deep`, plus a
  differential test that requires every result to equal a running node's but for head and
  LIB. [testdata/README.md](testdata/README.md) says how to run them.
- `tests/chain_api.rs` runs the chain API over TCP and reads every answer byte for byte: each
  endpoint by number and by id at the edges of the block logs and of `--range`, a block log
  reaching below the state history, and the real EOS block; dispatch on the whole target, any
  method, `OPTIONS`, illegal targets and the listings; the headers and keep-alive; empty,
  unparsable, oversized and slow bodies, slow heads, idle connections and answers left unread;
  full slots, a full budget, a body outgrowing its first reservation, large request bodies,
  and connections over the cap; blocks that do not decode or cannot be read; the refused
  configurations; a SHiP session streaming as before while API clients hammer
  `get_raw_block`; shutdown with a request in flight; and the metrics and `/health`.
- `tests/chain_api_nodeos.rs` sends the HTTP exchanges captured from a Spring 1.2.2 dev chain
  (`crates/core/testdata/chain-api`: the HTTP-layer cases and every request nodeos refused)
  to a server over a synthetic chain, and requires nodeos's whole answer, status line and
  headers in their order, but for the value of `Server` (a request without `Host` is answered
  as with one). Its ignored test replays a node's own captured answers against a server over a
  copy of that node's logs, and counts the [differences](#differences-from-nodeos) apart.

The core's strict readers are tested on their own with fault injection (torn tails, wrong
indexes, bad checksums, pruned and unsupported formats) and on real EOS and WAX entries and a
real EOS block from `crates/core/testdata/`. Its chain API tests answer every captured
dev-chain request from the captured blocks and compare status and body with nodeos's, and
render the real EOS block's four bodies byte for byte as Spring 1.2.1 and 1.2.2 nodes answered
them.

The ignored tests need logs and a node that CI does not have. The differential test has been
run against a Spring 1.2 node over a copy of its logs (v0 and v1, the full range, a `--range`
slice, and `--blocks none`). Before relying on a server, run `check --deep` on your copies and,
where you can, the differential test against a node holding the same blocks.

## Development

```sh
cargo fmt --package antelope-ship-archive --check
cargo clippy --locked --package antelope-ship-archive --all-targets -- -D warnings
cargo test --locked --package antelope-ship-archive
cargo test --locked --package hyperion-ship --features archive,testkit   # the core readers
cargo test --locked --package hyperion-ship --features chain-api,testkit # the chain API bodies
```

`.github/workflows/ship-archive.yml` runs these on Ubuntu and Windows. The crate is not
published to crates.io.

The readers, the catalog, the protocol codec and the vendored ABI live in the core
(`hyperion-ship`, feature `archive`); `scripts/extract_ship_abi.py` re-extracts the ABI from a
Spring checkout. The chain API's request parsing, block bodies and error bodies live there too
(feature `chain-api`); `scripts/capture_chain_api_goldens.sh` records what a Spring node
answers, for its goldens. This crate holds the server: `store.rs` (validation, served ranges,
building results), `session.rs` (the Spring session), `fetch.rs` (building ahead, the budget,
the io threads), `api/` (the chain API listener, its requests and `describe`), `frames.rs`,
`http.rs`, `metrics.rs`, `check.rs` and `cli.rs`.
