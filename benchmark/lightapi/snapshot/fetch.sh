#!/usr/bin/env bash
# Download the Libre snapshot once into the shared volume, extract the inner snapshot-<id>.bin, and
# derive its head block_num. Idempotent: skips work if /snapshot/READY exists.
set -euo pipefail

OUT=/snapshot
mkdir -p "$OUT"

if [[ -f "$OUT/READY" ]]; then
  echo "[fetch] snapshot already prepared:"
  cat "$OUT/READY"
  exit 0
fi

: "${SNAPSHOT_URL:?SNAPSHOT_URL is required}"
ARCHIVE="$OUT/snapshot-archive"
# Prefer a pre-placed local copy (benchmark/.snapshots, mounted at /local) to avoid re-downloading
# and tripping the provider's rate limit. Matched by the SNAPSHOT_URL basename.
LOCAL_ARCHIVE="/local/$(basename "$SNAPSHOT_URL")"
if [[ -f "$LOCAL_ARCHIVE" ]]; then
  echo "[fetch] using local copy $LOCAL_ARCHIVE (no download)"
  cp "$LOCAL_ARCHIVE" "$ARCHIVE"
else
  echo "[fetch] no local copy at $LOCAL_ARCHIVE — downloading $SNAPSHOT_URL"
  curl -fL --retry 5 --retry-delay 10 -o "$ARCHIVE" "$SNAPSHOT_URL"
fi

echo "[fetch] extracting snapshot .bin"
cd "$OUT"
case "$SNAPSHOT_URL" in
  *.tar.gz|*.tgz)   tar -xzf "$ARCHIVE" ;;
  *.tar.zst)        zstd -d -c "$ARCHIVE" | tar -xf - ;;
  *.bin.zst)        zstd -d -o snapshot.bin "$ARCHIVE" ;;
  *.tar)            tar -xf "$ARCHIVE" ;;
  *.bin)            cp "$ARCHIVE" snapshot.bin ;;
  *)                echo "[fetch] unknown archive type, trying tar" ; tar -xf "$ARCHIVE" || true ;;
esac

# Locate the snapshot bin (named snapshot-<block_id>.bin inside Antelope snapshot archives).
BIN="$(find "$OUT" -maxdepth 3 -name 'snapshot-*.bin' | head -1 || true)"
if [[ -z "$BIN" ]]; then
  BIN="$(find "$OUT" -maxdepth 3 -name '*.bin' ! -name 'snapshot-archive' | head -1 || true)"
fi
[[ -n "$BIN" ]] || { echo "[fetch] ERROR: no snapshot .bin found after extraction"; ls -la "$OUT"; exit 1; }

# Derive head block_num: Antelope snapshot files are snapshot-<64 hex block_id>.bin, where the first
# 8 hex chars of the block_id are the big-endian block height.
BASE="$(basename "$BIN")"
BLOCK_NUM=""
if [[ "$BASE" =~ snapshot-([0-9a-fA-F]{8}) ]]; then
  BLOCK_NUM=$((16#${BASH_REMATCH[1]}))
fi

echo "$BASE"      > "$OUT/SNAPSHOT_BIN"
echo "${BLOCK_NUM:-0}" > "$OUT/BLOCK_NUM"
{
  echo "snapshot_bin=$BASE"
  echo "snapshot_path=$BIN"
  echo "block_num=${BLOCK_NUM:-unknown}"
} > "$OUT/READY"

rm -f "$ARCHIVE"
echo "[fetch] prepared:"
cat "$OUT/READY"
