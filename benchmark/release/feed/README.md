# SHiP feed — keep WormDB live from your node (optional)

The `.wseg` segment is a point-in-time snapshot. This feed follows the chain in real time by consuming
your node's **State History (SHiP)** websocket directly — no MongoDB, no Hyperion. It decodes each
irreversible block's table deltas to learn which accounts changed, re-renders those accounts from the
node's chain API, and writes them into WormDB's live overlay (which shadows the segment). `/sync` then
reports the real lag.

## Requirements

- A node with `state_history_plugin` enabled (`state-history-endpoint`, default `:8080`) and the chain
  HTTP API reachable (default `:8888`).
- **[bun](https://bun.sh)** — install with `curl -fsSL https://bun.sh/install | bash`. The feed ships
  as a single self-contained `lightapi-ship-feed.js` (all dependencies bundled — no `bun install`).

## Run

```bash
cd feed

SHIP=ws://127.0.0.1:8080/ \
CHAIN_API=http://127.0.0.1:8888 \
CHAIN=wax \
PORT=6389 \
START=<block_num from step 2> \
bun lightapi-ship-feed.js
```

- `SHIP` — your node's `state-history-endpoint` (the State-History websocket).
- `CHAIN_API` — your node's `http-server-address` (chain HTTP API).
- `CHAIN` — must match the `chain` set in `wormdb.json`.
- `PORT` — WormDB's **WormWire** port (`6389` by default; that's `--port`, not the HTTP `:6390`).
- `START` — the `block_num` printed when you built the segment (step 2). The feed replays from there,
  so no change between the snapshot and now is missed. Omit (or set `0`) to start at the node's
  current LIB instead — only safe if you start the feed right after building the segment.

You'll see `[ship] block <N> bal=… acci=…` as blocks arrive, and:

```bash
curl http://localhost:6390/api/sync/wax     # "<delay> OK"  — grows if the feed stalls
```

## How it works (and why it's safe)

- **Irreversible-only** stream → applied state never needs rollback (no fork handling).
- SHiP is the *change-notifier*; the node's chain API (`get_account` / `get_currency_balance`) is the
  *renderer* — so it reuses the exact shapes the Light API serves, with no binary-delta decoding to
  maintain.
- Writes go to the KV overlay, which shadows the frozen segment per account. A periodic rebuild of the
  segment (re-run `snapshot-load --wseg` from a fresh snapshot, restart) drops the overlay and
  re-warms — bounding overlay growth.

## Notes

- nodeos rejects unknown `Host` headers by default; if the feed runs on a different host, set
  `http-validate-host = false` on the node (or run the feed where the API host matches).
