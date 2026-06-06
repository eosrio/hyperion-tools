#!/usr/bin/env bash
# Build the wormdb-server binary (with the lightapi + atomicassets domains composed) for Linux,
# into deploy/atomicassets/bin/wormdb. Reuses the proven 4-repo build flow (quic=false) from
# wormdb-server/docker/build-linux.sh: copy the four sibling repos (excluding caches), clone
# meshguard (the Windows symlink dep), then `zig build`.
#
#   REPOS_PARENT=P:/ ./build-wormdb.sh      # default REPOS_PARENT=P:/ (Docker Desktop on Windows)
#
# Needs: Docker, and the four repos as siblings under REPOS_PARENT:
#   wormdb  wormdb-server  wormdb-domain-lightapi  wormdb-domain-atomicassets
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/bin"
mkdir -p "$OUT"
REPOS_PARENT="${REPOS_PARENT:-P:/}"
# Docker Desktop on Windows needs a Windows-style mount source; a git-bash path (/p/...) isn't
# translated (it would bind to a non-existent path inside the Linux VM). cygpath -m -> P:/...
OUT_MOUNT="$(cygpath -m "$OUT" 2>/dev/null || echo "$OUT")"

echo "[build-wormdb] REPOS_PARENT=$REPOS_PARENT  OUT=$OUT_MOUNT"
docker run --rm \
  -v "${REPOS_PARENT}:/host:ro" \
  -v "${OUT_MOUNT}:/out" \
  -v wormdb-zigcache:/tmp/zc \
  -v wormdb-ziggcache:/tmp/zgc \
  ubuntu:24.04 bash -c '
set -e
ZIG_VER=0.16.0
echo "=== tools ==="
apt-get update -qq
apt-get install -y -qq curl xz-utils git build-essential cmake libsodium-dev ca-certificates pkg-config
echo "=== zig ${ZIG_VER} (sha256-pinned) ==="
# Pinned SHA-256 of zig-x86_64-linux-0.16.0.tar.xz (ziglang.org/download/index.json). Bump in lockstep with ZIG_VER.
ZIG_SHA=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
cd /opt && curl -fsSL "https://ziglang.org/download/${ZIG_VER}/zig-x86_64-linux-${ZIG_VER}.tar.xz" -o zig.tar.xz
echo "${ZIG_SHA}  zig.tar.xz" | sha256sum -c -
tar xf zig.tar.xz
export PATH="/opt/zig-x86_64-linux-${ZIG_VER}:${PATH}"
zig version
echo "=== copy 4 repos (sibling layout, excl caches/.git/node_modules) ==="
mkdir -p /work
for r in wormdb wormdb-server wormdb-domain-lightapi wormdb-domain-atomicassets; do
  mkdir -p "/work/$r"
  tar -C "/host/$r" --exclude=.zig-cache --exclude=zig-out --exclude=.git --exclude=node_modules -cf - . | tar -C "/work/$r" -xf -
done
echo "=== meshguard from github (replaces the windows symlink; commit-pinned) ==="
# Pinned meshguard commit — keeps the produced wormdb binary reproducible (bump deliberately).
MESHGUARD_SHA=56d9d8d44fbf6256632263e5021caa0f1575f54b
rm -rf /work/wormdb/deps/meshguard
git clone -q https://github.com/igorls/meshguard /work/wormdb/deps/meshguard
git -C /work/wormdb/deps/meshguard checkout -q "$MESHGUARD_SHA"
echo "=== zig build (quic=false) ==="
cd /work/wormdb-server
zig build --cache-dir /tmp/zc --global-cache-dir /tmp/zgc
cp -v zig-out/bin/wormdb /out/wormdb
echo "=== smoke ==="
mkdir -p /tmp/d
/out/wormdb --port 6599 --gateway-port 6599 --data /tmp/d --persistence none >/tmp/srv.log 2>&1 &
SRV=$!; sleep 2
grep "Composed domain" /tmp/srv.log || true
kill "$SRV" 2>/dev/null || true
'
echo "[build-wormdb] built: $OUT/wormdb"
