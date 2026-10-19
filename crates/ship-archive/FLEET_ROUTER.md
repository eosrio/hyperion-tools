# Deploy frozen SHiP archives with fleet-router

Use `antelope-ship-archive` for immutable history and
[fleet-router](https://github.com/eosrio/fleet-router) for the client-facing WebSocket
listeners. The [fleet-router deployment guide](https://eosrio.github.io/fleet-router/frozen-ship.html)
contains the router configuration and service setup; its
[Markdown source](https://github.com/eosrio/fleet-router/blob/main/docs/frozen-ship.md)
lives with the router. This companion covers preparing and operating the archive tier.

Use fleet-router **v0.7.0 or later** for this topology. Its stitched listener plans each leg
of a request and hands the client stream from one archive to the next, then to the live
tier, on one WebSocket connection. Older pool-only routing does not provide this behavior.
`antelope-ship-archive` is **preview**: qualify your copied logs and consuming client before
production use. See [Router behavior](#router-behavior).

## Example topology

All addresses below are illustrative private addresses. All nodes must belong to the same
chain. Ranges are half-open: `[1000001,2000001)` ends with block 2,000,000.

| Tier | SHiP upstream | Archive HTTP | Served range |
|---|---|---|---|
| archive-a | `ws://10.0.0.21:8080` | `http://10.0.0.21:9180` | `[1000001,2000001)` |
| archive-b | `ws://10.0.0.22:8080` | `http://10.0.0.22:9180` | `[2000001,3000001)` |
| live Spring node | `ws://10.0.0.31:8080` | existing node monitoring | begins at or below 3,000,001, then advances |

Clients use **`ws://10.0.0.10:17000`** for both archived and live blocks. Router monitoring
binds to `127.0.0.1:9090` on that host. Keep the live node's trace begin at or below
3,000,001, with matching coverage for the other payloads your client needs. Adjacent coverage
is sufficient; retain irreversible overlap where possible to make seam checks easier.
Retained files on neighboring archives may overlap while their **served** ranges above
remain disjoint. Add replicas of each archive range on separate hosts for availability.

These processes use separate files and roles: an archive needs frozen logs, their indexes,
disk and CPU; it does not run nodeos or replay chain state. Neither archive port provides
authentication or TLS. Restrict SHiP access to the router and HTTP access to operators and
monitoring on the private network. See the [archive CLI](src/cli.rs) and
[wire contract](CONTRACT.md).

## Prepare one immutable copy per range

1. Copy retained state-history files only after the producing node's LIB has **passed the
   last block in each copied file**, including any extra boundary files. Rotation alone
   does not establish finality. Do not copy a head log from a running writer. A stopped
   writer or a consistent filesystem snapshot must also contain only irreversible blocks
   for this use.
2. Include `trace_history`, `chain_state_history` when the client needs deltas, and
   `finality_data_history` when it needs v1 finality data, with their `.index` files. Include
   the block logs and indexes covering every served block. Copy whole retained files that
   contain the intended history; the server detects their actual coverage. Preserve
   boundary files for checkpoint verification. Use optional `--range` only when serving
   a subset of a larger copy.
3. Compare a file inventory and checksums before and after transfer, then mount the completed
   copy read-only. Keep the producer's originals until the copy passes validation. Do not
   modify, rotate, replace or append files while an archive process serves them.

For example, place archive-a's files in `/srv/archive/archive-a/state-history` and
`/srv/archive/archive-a/blocks`; use the corresponding `archive-b` directories on the second
host. The default `--blocks required` keeps each state-history range within block-log
coverage. Do not use `--blocks none` for clients that request the signed block.

The [input requirements](README.md#inputs), [store implementation](src/store.rs) and
[contract](CONTRACT.md#notation) define the accepted formats and immutable served ranges.
Pruned logs are not accepted. Each served log must be contiguous; an uncovered hole or a
conflicting overlap refuses startup. Different payload types can have different ranges:
trace coverage alone does not establish delta or finality coverage.

## Build, validate and serve

**No `--range` is required.** Both `check` and `serve` discover each log's range from the
file contents, not the filenames. With `--blocks required`, state-history coverage is
clipped to the block-log coverage. The default commands below serve the detected ranges;
inspect the output and confirm the example's A/B ranges before configuring the router.

Use `--range BEGIN-END` only for an intentional subset, such as splitting a larger copy
between servers or cutting at a snapshot block. Pass identical clipping options to
`check` and `serve`. This option does not rewrite the logs.

From a reviewed Hyperion Tools checkout, build and install the same binary on both archive
hosts:

```sh
cargo build --release --locked --package antelope-ship-archive
install -m 0755 target/release/antelope-ship-archive /usr/local/bin/antelope-ship-archive
```

On archive-a, validate the complete served range before starting its service:

```sh
antelope-ship-archive check \
  --dir /srv/archive/archive-a/state-history --dir /srv/archive/archive-a/blocks \
  --hash --strict

antelope-ship-archive serve \
  --dir /srv/archive/archive-a/state-history --dir /srv/archive/archive-a/blocks \
  --listen 10.0.0.21:8080 --http 10.0.0.21:9180 \
  --max-sessions 32 --memory-budget 268435456 --stream-threshold 1048576
```

On archive-b:

```sh
antelope-ship-archive check \
  --dir /srv/archive/archive-b/state-history --dir /srv/archive/archive-b/blocks \
  --hash --strict

antelope-ship-archive serve \
  --dir /srv/archive/archive-b/state-history --dir /srv/archive/archive-b/blocks \
  --listen 10.0.0.22:8080 --http 10.0.0.22:9180 \
  --max-sessions 32 --memory-budget 268435456 --stream-threshold 1048576
```

`--hash` implies `--deep`: it reads every served block and recomputes its header hash.
`--strict` also fails on warnings; investigate any warning before publishing the endpoint.
Startup validates the store before listening, but does not replace the full read. These
options are defined in the [CLI](src/cli.rs) and [checker](src/check.rs).

For missing or incorrect state-history indexes, rebuild into a **separate** directory with
`antelope-ship-archive index --dir PATH --out SIDECAR_DIR`. Pass the same
`--index-dir SIDECAR_DIR` to both `check` and `serve`. This command does not rebuild
block-log indexes. See [index recovery](README.md#missing-or-wrong-indexes).

Run the foreground `serve` command under your process supervisor, with read-only access to
the archive directories. The [Docker instructions](README.md#docker) provide a container
alternative. The archive session limit of 32 leaves headroom above the router's example
limit of 24 for its monitor and operator probes; account for every router using this archive.

## Connect the router and client

Use the [router deployment configuration](https://eosrio.github.io/fleet-router/frozen-ship.html)
to put all three upstreams on one stitched listener. Pin the real chain's 64-character
`chain_id`; stitched mode requires it. Set each archive's role and router-side capacity.
The router's JSON `range` is optional but recommended as a startup fallback: take its
values from the archive's verified `check` output or `/health`. It does not clip the
archive's reported or served coverage; only the archive's `--range` limits its served view.
For example, this is archive-a's entry in that listener's `servers`:

```json
{
  "name": "archive-a",
  "endpoint": "10.0.0.21:8080",
  "enabled": true,
  "role": "frozen",
  "range": { "begin": 1000001, "end": 2000001 },
  "max_sessions": 24
}
```

Declare archive-b the same way with `.22` and `[2000001,3000001)`. Set the live node's
`role` to `live`. Set `topology` to `stitched` explicitly, or leave its default `auto`, which
selects stitched mode when an enabled server is frozen. The role is never inferred from a
head that stopped moving. A declared range supports startup and offline coverage; reported
payload ranges still need to match your intended data. See the
[configuration reference](https://github.com/eosrio/fleet-router/blob/main/docs/configuration.md#topology-and-stitching).
For a fleet with replicas of both ranges and two live nodes, start from
[`example.stitched.config.json`](https://github.com/eosrio/fleet-router/blob/main/example.stitched.config.json).

The default `history_preference: "frozen"` keeps covered history reads on archives. The
default `predial_blocks: 2000` connects to the next leg ahead of the seam, and
`patch_head_lib: true` presents the fleet head and LIB in frozen results. These are
[router policy defaults](https://github.com/eosrio/fleet-router/blob/main/src/models.rs).

Use compatible SHiP ABIs across archives, live nodes and the client. The router selects a
listener ABI, or uses `stitching.abi_file`; the default `abi_mismatch: "warn"` logs
differences and keeps the upstream eligible, while `"exclude"` removes mismatches. Inspect
compatibility before using [`abi --from` and `serve --abi`](README.md#the-abi) to supply the
live node's reviewed ABI to an archive. ABI text alone cannot convert incompatible payloads.

The client connects once, reads the ABI and status, and sends a normal SHiP block request
with its fetch flags, exclusive end and acknowledgement flow. It can request from 1,000,001
through the live tier without reconnecting at 2,000,001 or 3,000,001. Persist completed block
numbers and IDs after committing their effects, as for an ordinary SHiP node. The router
clamps each upstream request to its planned leg, carries remaining credit and fetch flags
to the next leg, and sends no `have_positions` to frozen upstreams. See
[`build_request` and `positions_for`](https://github.com/eosrio/fleet-router/blob/main/src/session/link.rs).

At each seam, the next result's `prev_block.id` must continue the preceding delivered block.
A mismatch from an archive is a contract violation and triggers failover. A live mismatch
triggers failover when another upstream covers the cursor; with no alternative, it is
forwarded and logged as an error. When a frozen result omits its predecessor and this
session just delivered the immediately preceding irreversible block, the router can fill
that optional field from the delivered block. Keep the block logs and verify both seams
with your data; the router does not establish that all blocks inside a copied file were
irreversible. See
[seam behavior](https://github.com/eosrio/fleet-router/blob/main/docs/operations.md#seams-and-handoffs)
and the [session implementation](https://github.com/eosrio/fleet-router/blob/main/src/session/mod.rs).

For HTTP block queries, fleet-router v0.7 also supports a separate `chain_api` listener.
Enable the archive's `--api` on a separate private port and set its router `api_endpoint`
to that address. The archive's `--http` port is monitoring, not the chain API. See the
[archive Chain API](README.md#chain-api) and
[router Chain API guide](https://github.com/eosrio/fleet-router/blob/main/docs/chain-api.md).

## Verify before publishing

```sh
curl --fail http://10.0.0.21:9180/ready
curl --fail http://10.0.0.21:9180/health
curl --fail http://10.0.0.22:9180/ready
curl --fail http://10.0.0.22:9180/health
```

On the router host:

```sh
fleet-router config test --strict /etc/fleet-router/config.json
curl --fail http://127.0.0.1:9090/ready
curl --fail 'http://127.0.0.1:9090/ready?scope=history'
curl --fail http://127.0.0.1:9090/coverage
```

Archive `/ready` means the validated store is open and the listener accepts sessions.
`/health` reports the chain, actual per-payload served ranges, first block's predecessor,
last block, sessions and findings. Check these against the plan, including delta/finality
coverage. `/metrics` exposes Prometheus metrics. The [HTTP implementation](src/http.rs)
defines the archive responses.

With live nodes configured, router `/ready` tests tip availability and can stay green during
an archive outage. `?scope=history` also requires complete, online trace and chain-state
coverage, including known frozen ranges. `/coverage` shows per-log segments and their
upstreams. `config test --strict` probes the upstreams and rejects trace/chain-state holes
or a frozen server that has not reported its ranges. These checks do not replace a payload
and seam test with your real client. See
[readiness](https://github.com/eosrio/fleet-router/blob/main/src/health.rs).

Through port `17000`, request a short window crossing each seam, such as
`[1999998,2000004)` and `[2999998,3000004)`, with every payload your application uses.
Verify consecutive block numbers, IDs, predecessor links, normal acknowledgements and
completion at the exclusive end. Confirm the stream crosses each boundary on the same
client connection, then test mid-range reconnection from a saved checkpoint, an archive
outage and recovery, and the transition to live data. Keep each published archive immutable;
validate a replacement in a new directory and restart or replace its process before routing
clients to it.

## Router behavior

The [v0.7 planner](https://github.com/eosrio/fleet-router/blob/main/src/fleet/planner.rs)
requires a serving upstream to have every requested log the fleet has at the cursor. It
ends a leg where the upstream can no longer serve that request as fully as the fleet can.
Coverage is per payload; the router cannot supply deltas, finality data or signed blocks
that no upstream holds. Keep block logs for `fetch_block`: when no holder is available, a
blockless archive can still yield results without the signed block. Check the actual
payloads rather than accepting advertised trace coverage as sufficient.

An archive [stays open and silent at its end](CONTRACT.md#the-server-must). The router uses
its planned leg boundary to hand off, rather than waiting for a Close or a liveness timeout.
Normal handoffs incur no failure strike. `fleet_router_handoffs_total` and
`fleet_router_handoff_latency_seconds` expose these transitions.

When archived coverage exists but its upstreams are offline, quarantined, busy or full, the
session holds for `gap_wait_ms` (default **30,000 ms**), makes a final retry, then closes the
client with 1011 if the block is still unavailable. A known coverage hole closes with 1011
immediately. Holds for blocks served only by live nodes, and waits above the fleet head or
LIB gate, are unbounded. Unknown archive ranges can cause a hold instead of an immediate
gap. Review the exact
[holds, waits and gaps](https://github.com/eosrio/fleet-router/blob/main/docs/operations.md#holds-waits-and-gaps)
when setting client retries and monitoring.
