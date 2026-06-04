#!/usr/bin/env bash
set -euo pipefail
echo "[server] starting light-api on :7000 (config /etc/light-api.toml)"
exec light-api --config /etc/light-api.toml
