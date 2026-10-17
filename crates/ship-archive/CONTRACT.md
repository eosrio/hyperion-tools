# antelope-ship-archive wire contract

What a client of `antelope-ship-archive` can rely on, in particular a SHiP-aware router or
proxy that stitches several archive servers and live nodes into one history. Everything here
is the behavior of a Spring 1.x SHiP session (`plugins/state_history_plugin/.../session.hpp`)
over logs that never change, except where [Differences from a Spring node](#differences-from-a-spring-node)
says otherwise. The integration tests in `tests/` and the session's unit tests pin these
rules.

## Notation

For one server:

- `S_L = [b_L, e_L)` is the served range of log `L` ∈ {trace, chain_state, finality}: the
  blocks its files hold, cut to `--range`, and with `--blocks required` (the default) to the
  block-log range. It is `0,0` when the log is not served.
- `B = [b_B, e_B)` is the served block-log range: the blocks the block logs hold, cut to
  `--range`. It is not part of SHiP status; `/health` reports it.
- `I = ∪ S_L`: the blocks served from state history.
- `E = max e_L`, and `L* = E − 1` is the last servable block. `L* > 0`.
- `id(n)` is block `n`'s id, when the server knows it: from the entry header of the first
  served state-history log holding `n` (trace, then chain_state, then finality), else as
  block `n + 1`'s `previous` in the served block log. Every id read from an entry header has
  `num(id) == n`. There is no block 0: `id(0)` is never known, so block 1 never has a
  `prev_block`, as on a Spring node. `prev_block` also takes ids from files outside the
  served ranges (rule 6).

None of these change while the server runs.

## The server MUST

1. **Handshake.**
   - Accept the WebSocket upgrade on any path, with no extension negotiated (no
     permessage-deflate). A client has 10 s to complete the upgrade.
   - Send as the first message exactly one Text message holding the SHiP ABI: Spring's ABI
     text, byte for byte, or the `--abi` file. Every later message is Binary.
   - Set the HTTP header `Server: antelope-ship-archive/<version>` (informational).
2. **Capacity.** At `--max-sessions` open sessions, refuse the upgrade with HTTP 503 and
   `Retry-After: 1`. Never accept a session and then drop it for capacity.
3. **Request decoding**, with fc's semantics:
   - a varuint32 variant index; non-canonical encodings are accepted;
   - variants 0 `get_status_request_v0`, 1 `get_blocks_request_v0`, 2
     `get_blocks_ack_request_v0`, 3 `get_blocks_request_v1`, 4 `get_status_request_v1`;
   - bools must be 0 or 1; at most 1,048,576 `have_positions`; trailing bytes are ignored;
   - a Text request is decoded as a Binary one.

   A request that does not decode, or is larger than `--max-request-bytes` (default 1 MiB),
   drops the TCP connection **without** a Close frame.
4. **Status.**
   - `get_status_request_v0` is answered with `get_status_result_v0` (index 0, 121 bytes,
     `chain_id` always present); `get_status_request_v1` with `get_status_result_v1` (index 3,
     129 bytes).
   - `head = last_irreversible = {L*, id(L*)}`.
   - Each log's range is `[b_L, e_L)`, or `0,0` when not served.
   - `chain_id` is the block logs' chain id (or `--chain-id`).
   - A status reply costs no credit. Replies go out between result messages, never inside
     one: the requests queued when the session next looks are answered, in order, before the
     next result. One that arrives while a result is read or written follows that result.
   - More than 1024 queued status requests drop the TCP connection.
5. **get_blocks**, as Spring:
   - A request replaces the current one: credit = `max_messages_in_flight`, cursor =
     `start_block_num`.
   - `have_positions`: in the order given, each position below the current start whose id is
     unknown or different lowers the start to that position. The positions are then dropped.
     Known ids are `id(n)` and the ids `prev_block` names (rule 6), as Spring checks the
     positions with the ids it fills `prev_block` from.
   - An ack does `credit = credit.wrapping_add(n)`.
   - Send while `credit > 0 && cursor <= L* && cursor < end`; each result costs one credit and
     advances the cursor by one. `irreversible_only` changes nothing, as head = LIB.
   - A v0 request is answered with `get_blocks_result_v0` (index 1), a v1 request with
     `get_blocks_result_v1` (index 2), whatever `fetch_finality_data` is.
6. **Result content for block `n`:**
   - `head` and `last_irreversible`: `{L*, id(L*)}`.
   - `this_block = {n, id(n)}` iff `id(n)` is known.
   - `prev_block = {n − 1, id(n − 1)}` iff `this_block` is present and `id(n − 1)` is known.
     For the first block `n` of a served range `S_L`, when no served log knows `n − 1`, the
     id comes from any open file that names `n − 1`, served or not, as Spring fills
     `prev_block` from any log it has open: the state-history entry of `n − 1` (the catalogs
     reach one block below `--range` to keep the file holding it, when that file lines up),
     the block log's `previous` of `n`, or the hash of block `n − 1`'s header in the block
     log. With `--blocks required` the block log always names it. With
     `--verify-ids seams` every such file must give the same id, or the server refuses to
     start. With `--verify-ids off` only what gives the id is read: nothing below the range
     when the served block log names it, else the files in Spring's order up to the first
     that can be read, one that cannot being skipped with a startup warning. When no file
     names it (`--blocks none` over files that begin at `n`), that one result lacks its
     `prev_block`: startup logs a warning, and `check` and `/health`
     (`first.prev_block: null`) report it. Nothing warns for block 2: block 1, the genesis
     block, is in no state-history log, so no server sends it before block 2.
   - `block` iff `fetch_block`, `this_block` is present and `n ∈ B`: the stored
     `signed_block`, with a varuint32 length. Its `previous` is `prev_block`'s id, and its
     header hashes to `this_block`'s id.
   - `traces`, `deltas` and, for a v1 request with `fetch_finality_data`, `finality_data`:
     present iff requested and `n ∈ S_L` for that log, as the inflated entry payload,
     unchanged, with a varuint64 length; otherwise a single `0x00` byte.
   - A block whose id is unknown (in practice, below the served ranges) gets a result with
     head and LIB only. It still costs a credit and advances the cursor.
7. **Coverage honesty.** With `--blocks required`, every `n ∈ I` has `this_block`, and
   `block` when asked for. For every `n ∈ S_L`, a requested field of log `L` is present.
8. **End of range.** At `cursor >= min(end, E)`, send nothing and stay open: keep answering
   status, and accept a new request on the same connection.
9. **Data and integrity errors.**
   - Read and validate everything a result carries before writing its first frame: entry
     headers and position trailers, zlib framing, adler32, sizes, every log naming the same
     id for `n`, the block's bounds and trailer, its `previous` and its header hash. On any
     error, drop the TCP connection with no Close frame and no frame of that result.
   - A payload above `--stream-threshold` is validated by a full counting pass before the
     first frame, then inflated again while it is sent. Only an I/O error during that second
     pass ends a message midway, again with a TCP drop.
10. **Shutdown.** Send WebSocket Close 1001 to every session, after the message it is
    writing.
11. **Fragmentation.** A result is one fragmented Binary message:
    1. the variant index;
    2. the result base (head, LIB, `this_block`, `prev_block`, `block`), in frames of at most
       1 MiB when a large block makes it longer;
    3. for each payload field, its presence byte and length, then its bytes in frames of at
       most 1 MiB;
    4. an empty final frame.

    No frame is larger than 1 MiB. A status reply is one unfragmented Binary message. Frame
    boundaries carry no meaning (WebSocket stacks fragment differently): a client reassembles
    each message.
12. **Connection lifetime.** Never end an idle session: no idle timeout, no WebSocket
    ping. TCP keepalive (60 s idle, then probes every 10 s, 5 unanswered probes)
    ends the session of a peer that vanished without closing.

## The server MUST NOT

- Send a WebSocket Close of its own at the end of a range, or at any time other than
  shutdown. (It answers a client's Close.)
- Send a result for a block above `L*`, a keep-alive (a result that does not advance the
  cursor), or any result unasked.
- Skip a block, or move the cursor back, without a request.
- Send a status result unasked.
- Send a result while the credit is 0.
- Write to any input file or input directory. Every file is opened read-only.

## Invariants a client can check

From the rules above, a client can treat any of these as a fault of the server or of its
files, and fail over to a replica:

- a result for `n ∈ I` without `this_block`, or a keep-alive;
- `this_block.num` other than the cursor it expects;
- a requested field missing for `n ∈ S_L`;
- a result received while the client's credit is 0;
- a wrong result variant, a status result it did not ask for, or bytes that do not decode;
- a TCP drop without a Close frame.

A Close 1001 means the server is shutting down. HTTP 503 at the upgrade means it is busy, not
down. Status never changes, so one status reply tells a client `S_L`, `L*` and the chain id
for as long as the server runs.

## For a SHiP-aware router or proxy

- Request `[start, end)` with `end <= E`, so that completion can be counted: the request is
  complete once the result for `end − 1` arrives. Never rely on a Close, a drop, or silence:
  at the end the server simply idles.
- `max_messages_in_flight` may be 0, and acks may come later. A new request on an idle
  connection reuses it.
- `have_positions` is evaluated against this server's ids only. A position for a block it
  does not know (outside `I` and `B`, but for the block before a served range that its first
  block names as `prev_block`) moves the start back to that block, as on a node that lacks
  it, so send `[]` unless the positions are about blocks the server holds.
- Close a finished or abandoned connection with Close 1000.
- Several servers over copies of the same blocks, with the same `--abi`, send identical bytes:
  they are replicas.
- Block-log coverage is not visible in SHiP status. With `--blocks required` it covers every
  served block (rule 7); `/health` reports `B`.
- At a seam between two servers, the first result of the second carries the `prev_block`
  one server over all the logs would send (rule 6), so the router can check that the second
  server continues the first: its `prev_block` is the first server's `{L*, id(L*)}`.
  `/health` reports it beforehand as `first` (`{num, prev_block}`, `prev_block` `null`
  when no file names the block before).

## Differences from a Spring node

- **head = LIB = `L*`, always.** The logs are frozen, so the head never moves.
- **Integrity before the first frame.** Spring sends what its logs hold. This server first
  checks that every log names the same id for a block, that the block follows `prev_block`,
  and that its header hashes to `this_block`. A mismatch drops the connection before the
  result's first frame; Spring would omit or send the data.
- **Blocks above 20 MiB** (fc's limit for `block`) drop the connection before any frame of
  the result; Spring drops it after the variant index.
- **Ids from the block log** are `previous` of the next block, not the hash of the block's
  own header. So the block just below the first block of `B` has `this_block` (and nothing
  else), where a node holding the same block log would send no id.
- **Without block logs** (`--blocks none`), the first served block has no `prev_block` when
  no state-history file holds the block before it, where a node would name it from its
  block log. Startup warns, unless that block is block 2.
- **Limits:** at most 1024 queued status requests, requests up to `--max-request-bytes`, and
  10 s for the upgrade. Each drops the connection without a Close frame.
- **Close 1001 at shutdown**, after the message each session is writing.
