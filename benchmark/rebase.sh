#!/usr/bin/env bash
# rebase.sh — rebuild the WormDB Light-API frozen segment from the (Hyperion-maintained) live Mongo,
# swap it in, and re-stamp the feed watermark — dropping + re-warming the live overlay so it can't
# grow unbounded as accounts churn.
#
# The rebuild runs WHILE the old segment keeps serving; only the stop->swap->start is a brief gap
# (WormDB boots via mmap in <1s). Zero-downtime blue/green (build + warm a second instance, then swap
# traffic) is a future enhancement. The overlay lives in WormDB's persistence=none KV, so a restart
# clears it; the feed then resumes from the rebuild's baseline block, re-applying anything that
# changed during the build (re-rendering current state is idempotent — at-least-once is safe).
#
#   ./rebase.sh
# Env (defaults match the bench): CHAIN DB MONGO_C WORM_C SEG_HOST SEG_NEXT WSEG_BUILD PORT_WW PORT_HTTP META_TS FEED_TS
set -euo pipefail
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
here="$(cd "$(dirname "$0")" && pwd)"

CHAIN="${CHAIN:-wax}"
DB="${DB:-hyperion_wax}"
MONGO_C="${MONGO_C:-lightapi-bench-mongodb-1}"
WORM_C="${WORM_C:-wormdb-wax}"
SEG_HOST="${SEG_HOST:-$here/wax.wseg}"          # the file WormDB bind-mounts
SEG_NEXT="${SEG_NEXT:-$here/wax.next.wseg}"      # built here, then atomically renamed over SEG_HOST
WSEG_BUILD="${WSEG_BUILD:-$here/../target/release/wseg-build.exe}"
PORT_WW="${PORT_WW:-16489}"
PORT_HTTP="${PORT_HTTP:-16490}"
META_TS="${META_TS:-P:/wormdb/apps/bun/src/bin/lightapi-load-wax-meta.ts}"
FEED_TS="${FEED_TS:-P:/wormdb/apps/bun/src/bin/lightapi-feed.ts}"

log() { echo "[rebase $(date +%H:%M:%S)] $*"; }

# 1) Baseline = current Mongo head. The feed resumes here; the @-tables use @block_num, the rest block_num.
BASE=$(docker exec "$MONGO_C" mongosh "$DB" --quiet --eval '
  let m = 0;
  for (const n of ["accounts", "permissions", "eosio-userres", "eosio-delband"]) {
    const f = n.startsWith("eosio") ? "@block_num" : "block_num";
    const d = db.getCollection(n).find().sort({ [f]: -1 }).limit(1).toArray()[0];
    if (d) { const v = Number(d[f]); if (v > m) m = v; }
  }
  print(m);')
log "baseline block = $BASE"

# 2) Build the new segment from the current Mongo (the old segment keeps serving throughout).
#    wseg-build is a native exe — give it a native --out path (MSYS_NO_PATHCONV leaves /-paths alone).
SEG_NEXT_NATIVE="$(cygpath -w "$SEG_NEXT" 2>/dev/null || echo "$SEG_NEXT")"
log "building $SEG_NEXT (old segment still serving)..."
"$WSEG_BUILD" --db "$DB" --out "$SEG_NEXT_NATIVE" --chain "$CHAIN"

# 3) Swap + restart: stop releases the mmap, mv is atomic, start re-mmaps the new file and clears the
#    overlay (persistence=none). Brief gap.
log "swap + restart $WORM_C"
docker stop "$WORM_C" >/dev/null
mv -f "$SEG_NEXT" "$SEG_HOST"
docker start "$WORM_C" >/dev/null
until curl -s "http://127.0.0.1:$PORT_HTTP/api/usercount/$CHAIN" >/dev/null 2>&1; do sleep 0.3; done

# 4) Reload the KV meta (re-stamps segblock=BASE; persistence=none cleared it on restart).
SEGBLOCK="$BASE" PORT="$PORT_WW" bun "$META_TS" >/dev/null
log "done — overlay dropped, segment rebuilt at block $BASE"
log "resume the feed:  PORT=$PORT_WW bun $FEED_TS $BASE"
