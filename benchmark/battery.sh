#!/usr/bin/env bash
# WAX-scale benchmark battery for light-api. Host-native load via bench.mjs (no Docker NAT tax).
#
#   ./battery.sh [base] [concurrency] [durationSec] [account]
#
# Focuses on the DB-bound endpoints — the ones that actually exercise MongoDB at WAX scale.
# /networks is intentionally NOT in the loop: it's a serialize-from-memory list of configured
# chains, so it only measures axum + JSON overhead, not the database. We capture it ONCE below as a
# labeled framework-overhead baseline, then never again.
#
# TODO(freshness): once a live WAX node feeds Mongo, add /sync + /status timing against real
# block lag so we report freshness, not just throughput on a static snapshot.
set -euo pipefail
# Resolve script dir BEFORE disabling path conversion (MSYS_NO_PATHCONV mangles $0 → P:\p\...).
here="$(cd "$(dirname "$0")" && pwd)"
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'   # stop Git-Bash from mangling /api/... paths

B="${1:-http://127.0.0.1:7001}"
C="${2:-50}"
D="${3:-10}"
ACCT="${4:-waxupbitcold}"
run() { echo "### $1"; node "$here/bench.mjs" "$B" "$C" "$D" "$2"; }

echo "# baseline (framework overhead, captured once — not representative of DB load)"
run "/networks (cached meta — BASELINE ONLY)" "/api/networks"

echo "# DB-bound endpoints (the real WAX-scale workload)"
run "/usercount/wax (cached 21.75M-distinct aggregate)"        "/api/usercount/wax"
run "/balances/wax/$ACCT (indexed {scope} on 28M accounts)"    "/api/balances/wax/$ACCT"
run "/tokenbalance/wax/$ACCT/eosio.token/WAX (single doc)"     "/api/tokenbalance/wax/$ACCT/eosio.token/WAX"
run "/accinfo/wax/$ACCT (permissions+delband+userres join)"    "/api/accinfo/wax/$ACCT"
run "/account/wax/$ACCT (accinfo + balances, concurrent)"      "/api/account/wax/$ACCT"
run "/topholders/wax/eosio.token/WAX/100 (sorted index scan)"  "/api/topholders/wax/eosio.token/WAX/100"
run "/topram/wax/100 (indexed {ram_bytes:-1})"                 "/api/topram/wax/100"
# /topstake now sorts on the loader-emitted numeric `stake` (indexed {stake:-1}). Before that fix it
# sorted on a computed $split of the asset string — no index, full in-memory sort of all 21.75M
# userres rows, ~33s for a SINGLE request. Now ~2.7ms / 7.5K rps at c=50.
run "/topstake/wax/100 (indexed {stake:-1})"                   "/api/topstake/wax/100"
run "/holdercount/wax/eosio.token/WAX (cached count)"          "/api/holdercount/wax/eosio.token/WAX"
