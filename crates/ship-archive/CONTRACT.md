# antelope-ship-archive wire contract

What a client of `antelope-ship-archive` can rely on, in particular a SHiP-aware router or
proxy that stitches several archive servers and live nodes into one history. Everything here
is the behavior of a Spring 1.x SHiP session (`plugins/state_history_plugin/.../session.hpp`)
over logs that never change, except where [Differences from a Spring node](#differences-from-a-spring-node)
says otherwise. The integration tests in `tests/` and the session's unit tests pin these
rules. [Chain API](#chain-api) states the same for the nodeos `/v1/chain` block endpoints the
server answers with `--api`, and for an HTTP router or proxy in front of nodeos-compatible
APIs.

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

## Chain API

With `--api ADDR` the server also answers nodeos's `/v1/chain` block endpoints from its block
logs. This section is what an HTTP client can rely on there, in particular an HTTP router or
proxy in front of nodeos-compatible APIs that sends requests for old blocks to archive servers
and everything else to live nodes. "nodeos" below is a Spring node of a release from 1.1.0 to
1.2.2: these releases answer the block endpoints alike, error source lines included.
`tests/chain_api.rs`, `tests/chain_api_nodeos.rs` and the core's goldens (answers captured from
Spring 1.2.1 and 1.2.2 nodes, in `crates/core/testdata/chain-api`) pin these rules.

`B = [b_B, e_B)` is the served block-log range and `L*` the last servable block
([Notation](#notation)). With `--blocks required`, which `--api` needs, every `S_L` lies in `B`,
so `L* ∈ B`. `B` can begin below and end above the state-history ranges.

### The server MUST

1. **A listener of its own.** Serve the chain API only on the `--api` address, never on
   `--listen` or `--http`. Refuse to start when `--api` takes the port of `--listen` or
   `--http` (the same port on the same address, or on any address when either is
   unspecified), or with `--blocks none`.
2. **Block endpoints.** For every `n ∈ B`, answer `/v1/chain/get_block_info` (by number),
   `/v1/chain/get_block_header` (`include_extensions` false and true) and
   `/v1/chain/get_raw_block` (both by number or by the block's id) byte for byte as nodeos
   holding block `n` answers.
3. **Unknown Block.** Answer nodeos's `400 Unknown Block` (`error.code` 3100002) for every block
   number outside `B`, and for every id that is not the id of a block of `B` (an id is looked up
   at the number its first four bytes give). Never answer it for a block of `B` asked by number
   or by its own id.
4. **Requests and their errors, as nodeos with `verbose-http-errors` off.**
   - The empty-body rule: `400 Invalid Request`, "A Request body is required".
   - fc's legacy JSON parser and its field conversions: `400 Invalid Request`, "Unable to
     parse valid input from POST body".
   - `block_num_or_id` as a number cut to 32 bits, or as an id of an even number of hex
     digits, at most 64: `500 Invalid block ID` when it is empty, longer than 64 characters,
     or neither.
   - What an error echoes, cleaned as fc cleans it.
5. **HTTP, as nodeos's server.**
   - The request-target is checked first: one that is not origin-form, or holds `..`, gets
     `400 Illegal request-target`.
   - `OPTIONS`, on any path, gets `200 {}`.
   - The handler is the one whose path is the whole target, query string included.
   - Any method reaches the handler, with its body.
   - Every answer the server writes carries `Content-Type: application/json`, a
     `Content-Length`, title-case header names in nodeos's order,
     `Server: antelope-ship-archive/<version>`, and no `Date`.
   - A connection is kept alive as the request asks (HTTP/1.1 unless `close`, HTTP/1.0 with
     `keep-alive`).
6. **Listings, and nothing else.** `/v1/node/get_supported_apis` answers `{"apis":[...]}`: the
   three block endpoints, sorted. `/v1/archive/describe` is answered from memory, with no file
   read ([below](#v1archivedescribe)). Every other legal target gets nodeos's
   `404 Unknown Endpoint`, `get_info`, `get_block`, and the state, push and exec endpoints
   included.
7. **503 Busy, never 500, for what fails on the server's side.** Such a request gets `503`
   with the body nodeos sends when overloaded,
   `{"code":503,"message":"Busy","error":{"code":503,"name":"Busy","what":"<why>","details":[]}}`:
   - no request slot within `--api-queue-ms`: `Too many requests in flight: <n>`;
   - a memory budget that cannot hold the request: `Too many bytes in flight: <bytes used>`;
   - a block that cannot be read: `Block read failed`;
   - a block larger than 20 MiB, that does not decode, that holds another block number than it
     is stored under, or that renders past every bound: `Block data failed validation`;
   - a panic: `Internal error`.

   The only `500` answers are nodeos's `Invalid block ID` (rule 4).
8. **Limits.**
   - A body over `--api-max-body-bytes`, declared or as it arrives, gets `413` with
     `{"code":413,"message":"Payload Too Large","error":{"code":413,"name":"Payload Too Large","what":"Request body exceeds <N> bytes","details":[]}}`,
     and the connection is closed. So is a connection whose request body the memory budget
     cannot hold, after its `503`.
   - A connection whose request headers take over 10 s, or then whose body takes over 10 s, is
     closed unanswered. So is a kept-alive connection idle for 10 s, and one whose client takes
     none of its answer for 10 s.
   - Over `--api-max-connections`, a new connection is answered `503 Busy` (`Too many
     connections: <N>`) before its request is read, and closed.
9. **Identity.** `describe.chain_id` is the chain id of SHiP status, and `describe.last` is the
   SHiP head and LIB, `{L*, id(L*)}`. `get_block_info` for `last.num` answers with `id` equal to
   `last.id` whenever the logs agree (the block stored at `L*` hashes to `id(L*)`), so one
   request tells a client that the chain API and the SHiP listener serve the same blocks.
10. **Coverage.** `describe.blocks` is `B`: exactly the block numbers rule 2 answers.

### The server MUST NOT

- Check `Host`: a request with any `Host`, an empty one or none is answered.
- Send CORS headers.
- Send `Date`.
- Serve `get_info`, or any endpoint but those of rules 2 and 6 (`OPTIONS` gets `200 {}` on any
  path, as from nodeos).

### `/v1/archive/describe`

Any method, the body ignored; not a nodeos endpoint, and never listed by
`get_supported_apis`.

```json
{"server":"antelope-ship-archive/0.1.0","chain_id":"<64 hex>",
 "last":{"num":50000000,"id":"<64 hex>"},
 "blocks":{"begin":1,"end":50000001},
 "apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","/v1/chain/get_raw_block"],
 "get_block":"off","max_in_flight":4,"in_flight":0}
```

- `server`: the `Server` header's value.
- `chain_id`, `last`: rule 9. `blocks`: `B`, half-open (rule 10).
- `apis`: the block endpoints served, sorted, as `get_supported_apis` lists them.
- `get_block`: always `"off"`; `/v1/chain/get_block` is answered 404.
- `max_in_flight`: `--api-max-in-flight`; `in_flight`: the request slots taken now.

Only `in_flight` changes while the server runs.

### For an HTTP router or proxy

- Read `describe` for `B`, `L*`, the chain id and the request slots.
- Before routing to a server, check its identity: `describe.chain_id` is the chain's,
  `describe.last.id` is the head id its SHiP status reports, and `get_block_info` for
  `last.num` answers that id (rule 9).
- Send it only block requests whose block is in `describe.blocks` and whose endpoint is in
  `describe.apis`, with the body unchanged. Any `Host` is accepted.
- `400 Unknown Block` from it means that it does not hold that block: outside `B`, or an id
  that is not the id of the block of `B` at that number. `404` means it does not serve that
  endpoint. A node holding other blocks may answer either otherwise.
- `503` means that nothing was done: send the request again, to a replica or a live node.
  `200`, and the `400` and `500` parameter errors of rule 4, are what nodeos holding the block
  answers.
- Keep at most `describe.max_in_flight` requests in flight to it. More wait in its queue up to
  `--api-queue-ms`, then get `503`.
- Never send it `get_info`, state, push or exec requests: they get `404`.
- Servers over copies of the same block logs answer each block with the same bytes: they are
  replicas.

### Differences from nodeos

- **No `Host` check.** A request without a `Host`, or with an empty one, is answered as the
  same request with one; nodeos refuses it (`400 Disallowed HTTP HOST header in the request`),
  even with `http-validate-host = false`.
- **An oversized body** gets `413`, then the connection is closed; nodeos closes it without an
  answer.
- **A block that cannot be read or fails validation** gets `503 Busy`, so that a client asks
  elsewhere.
- **Several `Connection` lines** in an HTTP/1.1 request: the connection is closed once any line
  says `close`, and the answer says `Connection: close`; nodeos reads the first line alone and
  may keep it.
- **The request-target as hyper parses it.** A `#fragment` is dropped, so
  `/v1/chain/get_raw_block#x` is answered as `/v1/chain/get_raw_block` (nodeos answers 404), and
  a `..` after the `#` escapes the illegal-target check. A byte outside the URI grammar (`<`,
  `>`, a backquote, a control or non-ASCII byte) gets hyper's own `400`, with an empty body and
  no `Content-Type` or `Server`, and the connection is closed. Clients send neither.
- **HEAD** is answered with the head alone; nodeos sends the body too. The interim
  `100 Continue` to `Expect: 100-continue` carries no `Server` header.
- **`Server`** is `antelope-ship-archive/<version>`.
- **Error `line_number`s** are Spring 1.1.0 to 1.2.2's; other releases put these errors on
  other source lines. `verbose-http-errors` is not mirrored.
