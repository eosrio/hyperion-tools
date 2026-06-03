#!/usr/bin/env bash
# Boot nodeos: on first run, load the shared Libre snapshot; afterwards resume from the data dir.
# Peers from LIBRE_P2P_PEERS let nodeos advance past the snapshot block so SHiP makes it irreversible
# (Chronicle needs that to read the full-state delta).
set -euo pipefail

DATA=/srv/libre/data
ETC=/srv/libre/etc
mkdir -p "$DATA" "$ETC"

# Build config.ini: the template already carries default Libre seed peers; append any extras from env.
grep -v '##PEERS##' /etc/nodeos/config.ini.template > "$ETC/config.ini"
DEFAULT_PEERS="$(grep -c '^p2p-peer-address' "$ETC/config.ini" || echo 0)"
EXTRA=0
for p in $(echo "${LIBRE_P2P_PEERS:-}" | tr ',; ' '\n' | sed '/^$/d'); do
  echo "p2p-peer-address = $p" >> "$ETC/config.ini"
  EXTRA=$((EXTRA + 1))
done
echo "[nodeos] peers: $DEFAULT_PEERS bundled + $EXTRA from LIBRE_P2P_PEERS"

SNAP_ARGS=()
if [[ -d "$DATA/state" ]]; then
  echo "[nodeos] existing chain state found — resuming (no snapshot)"
else
  BIN="$(find /snapshot -name 'snapshot-*.bin' | head -1 || true)"
  [[ -n "$BIN" ]] || BIN="$(find /snapshot -name '*.bin' ! -name '*archive*' | head -1 || true)"
  [[ -n "$BIN" ]] || { echo "[nodeos] ERROR: no snapshot .bin in /snapshot"; exit 1; }
  echo "[nodeos] first boot — loading snapshot $BIN"
  SNAP_ARGS=(--snapshot "$BIN")
fi

echo "[nodeos] starting (peers=$((DEFAULT_PEERS + EXTRA)), SHiP on :8080, chain API on :8888)"
exec nodeos --data-dir "$DATA" --config-dir "$ETC" --disable-replay-opts "${SNAP_ARGS[@]}"
