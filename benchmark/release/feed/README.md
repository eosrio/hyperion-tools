# SHiP feed — keep WormDB live from your node (optional)

The `.wseg` segment is a point-in-time snapshot. This feed follows the chain in real time by consuming
your node's **State History (SHiP)** websocket directly — no MongoDB, no Hyperion. It decodes each
irreversible block's table deltas to learn which accounts changed, re-renders those accounts from the
node's chain API, and writes them into WormDB's live overlay (which shadows the segment). `/sync` then
reports the real lag.

## Requirements

- A node with `state_history_plugin` enabled (`state-history-endpoint`, default `:8080`) and the chain
  HTTP API reachable (default `:8888`).
- **[bun](https://bun.sh)** (`curl -fsSL https://bun.sh/install | bash`) — or use the prebuilt
  `lightapi-ship-feed` binary in this folder if present.

## Run

```bash
cd feed
bun install        # first time (pulls @wharfkit/antelope)

SHIP=ws://127.0.0.1:8080/ \
CHAIN_API=http://127.0.0.1:8888 \
CHAIN=wax \
PORT=6389 \
bun src/bin/lightapi-ship-feed.ts
```

(`PORT` is WormDB's WormWire port — 6389 by default. `CHAIN` must match the chain you configured.)

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
