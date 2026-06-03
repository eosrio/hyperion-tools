#!/usr/bin/env bash
# Load-test the same endpoint on both APIs and print throughput/latency. Prefers `oha`, then `hey`,
# then ApacheBench `ab`, then a crude bash fallback.
#
#   ./benchmark.sh
#   N=5000 C=100 ACCT=eosio.token ./benchmark.sh
#   PATH_Q=/api/account/libre/eosio ./benchmark.sh
set -uo pipefail

OURS_URL="${OURS_URL:-http://localhost:7000}"
CC_URL="${CC_URL:-http://localhost:5001}"
CHAIN="${CHAIN:-libre}"
ACCT="${ACCT:-eosio.token}"
PATH_Q="${PATH_Q:-/api/balances/$CHAIN/$ACCT}"
N="${N:-2000}"
C="${C:-50}"

bash_bench() {
  local url="$1" ok=0 start end
  start="$(date +%s.%N)"
  for ((i = 0; i < N; i++)); do
    curl -fsS -o /dev/null "$url" && ok=$((ok + 1))
  done
  end="$(date +%s.%N)"
  awk -v n="$N" -v ok="$ok" -v s="$start" -v e="$end" \
    'BEGIN { d = e - s; printf "  %d/%d ok in %.2fs -> %.0f req/s (sequential)\n", ok, n, d, (d>0?n/d:0) }'
}

run() {
  local name="$1" base="$2" url="$2$PATH_Q"
  echo "== $name : $url  (n=$N c=$C) =="
  # sanity: one request first
  if ! curl -fsS -o /dev/null "$url"; then
    echo "  endpoint not reachable / returned error — skipping"; echo; return
  fi
  if   command -v oha >/dev/null; then oha -n "$N" -c "$C" --no-tui "$url"
  elif command -v hey >/dev/null; then hey -n "$N" -c "$C" "$url"
  elif command -v ab  >/dev/null; then ab -n "$N" -c "$C" "$url"
  else echo "  (install oha/hey/ab for concurrent load; using sequential fallback)"; bash_bench "$url"
  fi
  echo
}

run "OURS (light-api)"      "$OURS_URL"
run "cc32d9 (Perl/Starman)" "$CC_URL"
