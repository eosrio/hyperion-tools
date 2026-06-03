# WormDB Light API — Preview (early testers)

Serve the **cc32d9 `eosio_light_api`** HTTP API for any Antelope chain from a single static binary +
one file built straight from a chain snapshot. No MongoDB, no Hyperion, no node required to serve.

This preview ships two Linux x86-64 binaries:

- **`snapshot-load`** — decodes a chain snapshot directly into a WormDB segment (`.wseg`).
- **`wormdb`** — serves the Light API from that segment (memory-mapped).

> WormDB is currently a private project, so the `wormdb` binary is shipped prebuilt. `snapshot-load`
> is open source — see github.com/eosrio/hyperion-tools.

---

## 0. One-time setup

```bash
tar xzf wormdb-lightapi-preview.tar.gz
cd wormdb-lightapi-preview
chmod +x bin/*
sudo apt-get install -y libsodium23     # wormdb's only runtime dependency
mkdir -p data
```

## 1. Get a chain snapshot

Any portable Antelope snapshot `.bin` (v2–v8). For example WAX from EOS Nation, or your own node's
`snapshot-*.bin`. `.bin.zst` works too (decompressed automatically).

```bash
# example — substitute your chain's snapshot
curl -L -o data/snap.bin.zst <snapshot-url>
```

## 2. Build the segment (no Mongo, no node)

```bash
./bin/snapshot-load --snapshot data/snap.bin --tables lightapi --chain wax --wseg data/chain.wseg
```

It prints `head block_num=…` (note it for step 3) and writes `data/chain.wseg`. A WAX snapshot
(21.7M accounts) builds in a few minutes; small chains in seconds.

## 3. Configure your chain

Edit **`wormdb.json`** → the `lightapi.networks[0]` entry. The shipped file is pre-filled for WAX;
for another chain set `chain`, `systoken`, `decimals`, `chainid`, `description`, `rex_enabled`, and
`block_num` (the value printed in step 2). Reference values for common chains:

| chain | systoken | decimals | chainid (first 8) | rex |
|---|---|---|---|---|
| wax | WAX | 8 | 1064487b… | no |
| eos | EOS | 4 | aca376f2… | yes |
| telos | TLOS | 4 | 4667b205… | yes |
| libre | LIBRE | 4 | 38b1d781… | no |

(Full chainids: run `snapshot-load --inspect` on the snapshot, or use your chain's `get_info`.)

## 4. Serve

```bash
./bin/wormdb --config wormdb.json
```

WormDB mmaps the segment (boots in under a second, even for a multi-GB WAX segment) and serves on
**:6390**.

## 5. Query

```bash
curl http://localhost:6390/api/balances/wax/<account>
curl http://localhost:6390/api/accinfo/wax/<account>
curl http://localhost:6390/api/account/wax/<account>
curl http://localhost:6390/api/topholders/wax/eosio.token/WAX/100
curl http://localhost:6390/api/networks
```

All 16 cc32d9 HTTP endpoints are served (`balances tokenbalance accinfo account topholders topram
topstake holdercount usercount codehash key rexbalance rexraw sync status networks`),
byte-compatible with the reference implementation. `balances`, `accinfo`, `account`, `topholders`,
`holdercount`, `usercount`, and `networks` work fully from the segment alone.

## 6. WebSocket API (cc32d9 JSON-RPC)

The same port speaks the cc32d9 WebSocket API (JSON-RPC 2.0, `jsonrpc2-ws` dialect). Connect to
`ws://localhost:6390/` and call any of the 4 streaming methods — results arrive as `reqdata`
notifications, one per row, ending with `{end:true,status:200}`:

```js
const ws = new WebSocket("ws://localhost:6390/");
ws.onopen = () => ws.send(JSON.stringify({
  jsonrpc:"2.0", id:1, method:"get_token_holders",
  params:{ reqid:1, network:"wax", contract:"eosio.token", currency:"WAX" }
}));
ws.onmessage = (e) => console.log(JSON.parse(e.data).params);  // {method,reqid,data}|{end:true}
```

Methods: `get_networks`, `get_balances` (`{network, accounts[]≤100}`), `get_token_holders`
(streams every holder), `get_accounts_from_keys` (`{network, keys[]≤100}`).

---

## What you get

- **Tiny footprint** — a multi-GB segment serves at tens of MiB resident (the OS pages in only the
  working set). A whole chain's Light API in one process.
- **Instant boot** — `mmap`, not load. Restart is sub-second.
- **Flat throughput** — every endpoint, including the 6-source `account` assembly, serves at the same
  rate, because it assembles in-process.

## Optional: keep it live

The segment is a point-in-time snapshot. To follow the chain in real time, run the **SHiP feed**
(`feed/`) against your node's State History websocket — it writes only changed accounts into WormDB's
live overlay, and `/sync` reports the lag. See `feed/README.md`. (Requires a node with
`state_history_plugin` enabled.)

## Notes & limits (preview)

- Linux x86-64 only in this drop. `wormdb` needs `libsodium23` (one apt package).
- Balance/permission **array order** may differ from the reference (same data, different order).
- `block_num`/`sync` are static until the live feed runs.
- `topram`/`topstake` (resource rankings) and HTTP `/key` populate from the live feed; the WebSocket
  `get_accounts_from_keys` serves keys from the segment directly. `/rexbalance`/`/rexraw` apply to
  rex-enabled chains.
- Feedback welcome — this is an early preview.
