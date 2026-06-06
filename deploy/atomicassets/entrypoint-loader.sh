#!/usr/bin/env bash
# One-shot loader: decode the snapshot's AtomicAssets state into MongoDB (snapshot-load --tables
# atomic), then build the faceted .wseg segment from that Mongo state (aa-build).
set -euo pipefail

CHAIN="${CHAIN:-jungle4}"
MONGO_URI="${MONGO_URI:-mongodb://mongodb:27017}"
MONGO_PREFIX="${MONGO_PREFIX:-hyperion}"
MONGO_DB="${MONGO_PREFIX}_${CHAIN}"           # snapshot-load db name = <prefix>_<chain>
WSEG_OUT="${WSEG_OUT:-/data/aa.wseg}"
AA_DATA_FIELDS="${AA_DATA_FIELDS:-rarity}"
ASSET_LIMIT="${ASSET_LIMIT:-0}"

# Prefer the EOS-Nation-named portable snapshot (its trailing digits are the block_num that
# snapshot-load auto-derives); fall back to any non-archive .bin. Selection is DETERMINISTIC: sort the
# matches and take the lexically-newest (the date/height is in the name), warning if there's more than one.
mapfile -t BINS < <(find /snap -maxdepth 1 -name 'snapshot-*.bin' | LC_ALL=C sort)
[[ ${#BINS[@]} -gt 0 ]] || mapfile -t BINS < <(find /snap -maxdepth 1 -name '*.bin' ! -name '*archive*' | LC_ALL=C sort)
[[ ${#BINS[@]} -gt 0 ]] || { echo "[loader] ERROR: no snapshot .bin in /snap (mount SNAPSHOT_DIR)"; ls -la /snap; exit 1; }
if [[ ${#BINS[@]} -gt 1 ]]; then
  echo "[loader] WARN: ${#BINS[@]} snapshots in /snap — picking the lexically-newest:"
  printf '[loader]   %s\n' "${BINS[@]}"
fi
BIN="${BINS[-1]}"

echo "[loader] snapshot=$BIN chain=$CHAIN -> $MONGO_URI db=$MONGO_DB"

# 1) snapshot -> Mongo: atomicassets + atomicmarket state, decoded (seek path; the atomic preset
#    needs a local .bin). Drops the AA/AM collections first for an idempotent re-run.
echo "[loader] === snapshot-load (--tables atomic) ==="
time snapshot-load --snapshot "$BIN" --tables atomic --chain "$CHAIN" \
  --mongo "$MONGO_URI" --mongo-prefix "$MONGO_PREFIX" --mongo-drop

# 2) Mongo -> .wseg: the faceted AtomicAssets segment the Zig domain serves.
echo "[loader] === aa-build -> $WSEG_OUT ==="
AABUILD_ARGS=(--mongo-uri "$MONGO_URI" --db "$MONGO_DB" --out "$WSEG_OUT" --data-fields "$AA_DATA_FIELDS")
if [[ "$ASSET_LIMIT" =~ ^[0-9]+$ ]] && [[ "$ASSET_LIMIT" -gt 0 ]]; then
  AABUILD_ARGS+=(--limit "$ASSET_LIMIT")
fi
time aa-build "${AABUILD_ARGS[@]}"

echo "[loader] done:"
ls -la "$WSEG_OUT"
