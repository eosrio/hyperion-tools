#!/usr/bin/env bash
# Serve the AtomicAssets HTTP API from the mmap'd .wseg segment. The wormdb-server binary already
# composes the lightapi + atomicassets domains; attaching a segment named `atomicassets` lights up
# the AA routes (the domain mounts under that segment name).
set -euo pipefail

AA_SEGMENT="${AA_SEGMENT:-/data/aa.wseg}"
if [[ ! -f "$AA_SEGMENT" ]]; then
  echo "[server] ERROR: segment $AA_SEGMENT missing — run aa-loader first"
  exit 1
fi
mkdir -p /var/lib/wormdb

echo "[server] serving AtomicAssets from $AA_SEGMENT  (gateway :6390, wormwire :6389)"
exec wormdb --config /etc/wormdb.json --atomicassets-segment "$AA_SEGMENT"
