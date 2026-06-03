#!/usr/bin/env bash
# Compare our light-api against the cc32d9 reference API endpoint-by-endpoint for a list of accounts.
# Volatile fields (the live chain{} block) are stripped before diffing; balances are key-sorted.
#
#   ./parity-check.sh [accounts-file]
#   VERBOSE=1 ./parity-check.sh            # show the diff on mismatch
#   OURS_URL=http://host:7000 CC_URL=http://host:5001 ./parity-check.sh
#
# Requires: curl, jq.
set -uo pipefail

OURS_URL="${OURS_URL:-http://localhost:7000}"
CC_URL="${CC_URL:-http://localhost:5001}"
CHAIN="${CHAIN:-libre}"
ACCTS_FILE="${1:-$(dirname "$0")/accounts.txt}"

command -v jq >/dev/null   || { echo "jq is required"; exit 1; }
command -v curl >/dev/null || { echo "curl is required"; exit 1; }

# Normalizers: stable, comparable views of each endpoint.
norm_balances() { jq -S 'sort_by(.contract, .currency)'; }
norm_accinfo()  { jq -S 'del(.chain)'; }                 # drop the volatile live block header
norm_account()  { jq -S 'del(.chain) | .balances |= sort_by(.contract, .currency)'; }

pass=0; fail=0; err=0

compare() {
  local label="$1" path="$2" norm="$3"
  local a b
  a="$(curl -fsS "$OURS_URL$path" 2>/dev/null | "$norm" 2>/dev/null)" || a="__OURS_ERR__"
  b="$(curl -fsS "$CC_URL$path"   2>/dev/null | "$norm" 2>/dev/null)" || b="__CC_ERR__"
  if [[ "$a" == "__OURS_ERR__" || "$b" == "__CC_ERR__" || -z "$a" || -z "$b" ]]; then
    echo "  ERR  $label (ours=${a:0:12} cc=${b:0:12})"; err=$((err + 1)); return
  fi
  if [[ "$a" == "$b" ]]; then
    echo "  PASS $label"; pass=$((pass + 1))
  else
    echo "  DIFF $label"; fail=$((fail + 1))
    if [[ "${VERBOSE:-0}" == "1" ]]; then
      diff <(printf '%s' "$a") <(printf '%s' "$b") | sed 's/^/      /' | head -60
    fi
  fi
}

echo "ours=$OURS_URL  cc32d9=$CC_URL  chain=$CHAIN"
echo "(< = ours, > = cc32d9)"
echo
while IFS= read -r acct; do
  acct="${acct%%#*}"; acct="$(echo -n "$acct" | tr -d '[:space:]')"
  [[ -z "$acct" ]] && continue
  echo "account: $acct"
  compare "balances" "/api/balances/$CHAIN/$acct" norm_balances
  compare "accinfo"  "/api/accinfo/$CHAIN/$acct"  norm_accinfo
  compare "account"  "/api/account/$CHAIN/$acct"  norm_account
done < "$ACCTS_FILE"

echo
echo "================ PASS=$pass  DIFF=$fail  ERR=$err ================"
[[ "$fail" -eq 0 && "$err" -eq 0 ]]
