#!/usr/bin/env bash
# Refresh + publish the WAX contract-ABI snapshot as a GitHub Release asset,
# stamped by the last indexed block. Run on a host that has: a WAX state-history
# node, the abi-scanner binary, zstd, python3, and an authenticated `gh`.
#
# Re-run anytime — the --checkpoint makes the scan incremental, so a weekly run
# only indexes the new blocks, then republishes the (small) snapshot.
set -euo pipefail

STATE_HISTORY="${STATE_HISTORY:-/data/nodeos/state-history}"
SCANNER="${SCANNER:-abi-scanner}"
REPO="${REPO:-eosrio/abi-scanner}"
THREADS="${THREADS:-8}"
OUT="${OUT:-wax-abi.ndjson}"
CKPT="${CKPT:-wax-abi.ckpt}"

# 1. scan / resume forward (incremental thanks to --checkpoint)
"$SCANNER" --from-disk "$STATE_HISTORY" --start 2 --end 999999999 \
  --threads "$THREADS" --out "$OUT" --checkpoint "$CKPT"

# 2. last indexed block = checkpoint watermark - 1
LAST=$(( $(cat "$CKPT") - 1 ))

# 3. clean (drop any malformed line) + de-dupe by (block, account) for a pristine asset
python3 - "$OUT" "$OUT.clean" <<'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
seen = set(); n = bad = dup = 0
with open(dst, "w") as o:
    for line in open(src):
        line = line.rstrip("\n")
        if not line:
            continue
        try:
            d = json.loads(line)
        except Exception:
            bad += 1; continue
        k = (d.get("block"), d.get("account"))
        if k in seen:
            dup += 1; continue
        seen.add(k); o.write(line + "\n"); n += 1
print(f"clean={n} dropped_malformed={bad} dropped_duplicate={dup}", file=sys.stderr)
PY

# 4. compress (stable asset name -> stable releases/latest/download URL)
zstd -19 -T0 -q -f "$OUT.clean" -o wax-abi.ndjson.zst

# 5. publish a release stamped by the last indexed block
gh release create "wax-abi-$LAST" wax-abi.ndjson.zst --repo "$REPO" \
  --title "WAX ABI snapshot @ block $LAST" \
  --notes "WAX contract-ABI history indexed through block $LAST. See datasets/README.md for usage and catch-up."

echo "published wax-abi-$LAST -> https://github.com/$REPO/releases/tag/wax-abi-$LAST"
