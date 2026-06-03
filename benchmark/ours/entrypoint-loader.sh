#!/usr/bin/env bash
# One-shot: load the Libre snapshot's full Light-API state into MongoDB via `--tables lightapi`
# (voters, accounts, eosio system tables, permissions + pub_keys reverse index).
set -euo pipefail

CHAIN="${CHAIN:-libre}"
MONGO_URI="${MONGO_URI:-mongodb://mongodb:27017}"

BIN="$(find /snapshot -name 'snapshot-*.bin' | head -1 || true)"
[[ -n "$BIN" ]] || BIN="$(find /snapshot -name '*.bin' ! -name '*archive*' | head -1 || true)"
[[ -n "$BIN" ]] || { echo "[loader] ERROR: no snapshot .bin in /snapshot"; ls -la /snapshot; exit 1; }

BLOCK="$(cat /snapshot/BLOCK_NUM 2>/dev/null || echo 0)"
echo "[loader] snapshot=$BIN chain=$CHAIN block=$BLOCK -> $MONGO_URI"

ARGS=(--snapshot "$BIN" --chain "$CHAIN" --tables lightapi --mongo "$MONGO_URI" --mongo-drop)
if [[ "$BLOCK" =~ ^[0-9]+$ ]] && [[ "$BLOCK" -gt 0 ]]; then
  ARGS+=(--block-num "$BLOCK")
fi

time snapshot-load "${ARGS[@]}"
echo "[loader] done."
