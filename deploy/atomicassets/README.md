# WormDB AtomicAssets — end-to-end stack (Track B / v3)

Runs the **Rust + WormDB** AtomicAssets serving lifecycle on Docker, fed by **Jungle 4 testnet**:
`snapshot → Mongo → .wseg → serve` now (Phase 0–1), then a live SHiP feed (Phase 2) and a
Hyperion/`archive-server` history tier (Phase 3). See the design plan for the full roadmap.

## Tiers (store each datum once, eventually)

- **State** → WormDB `.wseg` + overlay **and** MongoDB (dual-homed *on purpose* while we prove WormDB —
  the two are diffed; we don't blacklist Hyperion's AA indexing yet).
- **History index** → Hyperion `<chain>-action-*` with an `@atomic` enrichment (Phase 3).
- **History payloads** → cold `archive-server`, decoded lazily from the frozen ship logs (Phase 3).

## Chain source

The existing **Jungle 4 node in WSL** (`~/chains/jungle`, tmux): chain API `:28888`, SHiP `:28080`, with a
ready snapshot and the state-history logs. Containers reach it at `host.docker.internal:28888/:28080`
(the node has `http-validate-host` on — send a `Host` header). No nodeos container.

## Phase 0 — bring up the state pipe

```sh
# 1. Build the wormdb-server binary (4-repo Zig build: engine + lightapi + atomicassets domains).
./build-wormdb.sh                      # -> bin/wormdb   (REPOS_PARENT=P:/ by default)

# 2. Point at the snapshot + configure.
cp .env.example .env
#   set SNAPSHOT_DIR to a host dir holding snapshot-…-jungle4-v8-*.bin

# 3. Up: aa-loader (snapshot -> Mongo -> aa.wseg) then wormdb-aa serves it.
docker compose --profile state up --build

# 4. Query.
curl "http://localhost:6390/atomicassets/v1/assets?owner=<account>&limit=10"
```

## Layout

| file | role |
|------|------|
| `docker-compose.yml` | `mongodb` + `aa-loader` (one-shot) + `wormdb-aa` (serve) |
| `Dockerfile.tools` | Rust image: `snapshot-load` + `aa-build` |
| `Dockerfile.wormdb` | slim runtime; copies the prebuilt `bin/wormdb` |
| `build-wormdb.sh` | builds `bin/wormdb` (reuses `wormdb-server/docker/build-linux.sh` flow) |
| `entrypoint-loader.sh` | `snapshot-load --tables atomic` → `aa-build` |
| `entrypoint-server.sh` | `wormdb --atomicassets-segment /data/aa.wseg` |
| `wormdb.json` | server/gateway config (segment attached via the CLI flag) |

`bin/`, `snapshot/`, `.env`, `*.wseg` are gitignored.
