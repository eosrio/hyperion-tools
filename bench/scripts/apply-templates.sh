#!/usr/bin/env bash
# Apply the Hyperion-shaped index templates for $CHAIN to a LOCAL Elasticsearch.
#
#   ./scripts/apply-templates.sh            # uses ../.env (CHAIN, ES)
#   CHAIN=eos ES=http://localhost:9200 ./scripts/apply-templates.sh
#
# SAFETY: refuses any ES host that is not loopback unless BENCH_ALLOW_EXTERNAL_ES=1 — the write
# benchmark must run against a throwaway local cluster, never production.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
# Read just the keys we need from .env (env vars win), robust to spaces/quotes in other values.
envfile() { [ -f "$root/.env" ] && grep -E "^$1=" "$root/.env" | tail -1 | cut -d= -f2- | sed -E 's/^["'"'"']//; s/["'"'"']$//'; }

ES="${ES:-$(envfile ES)}"; ES="${ES:-http://localhost:9200}"
CHAIN="${CHAIN:-$(envfile CHAIN)}"; CHAIN="${CHAIN:-wax}"
# Optional index.mode (e.g. INDEX_MODE=logsdb for ~6% extra storage savings on ES >= 8.17 / 9.x).
MODE="${INDEX_MODE:-$(envfile INDEX_MODE)}"

host="$(printf '%s' "$ES" | sed -E 's#^https?://##; s#[:/].*$##')"
case "$host" in
  localhost|127.0.0.1|::1|"") ;;
  *) if [ "${BENCH_ALLOW_EXTERNAL_ES:-0}" != "1" ]; then
       echo "REFUSING: ES host '$host' is not loopback. This stack is for LOCAL benchmarking only." >&2
       echo "If you really mean it, set BENCH_ALLOW_EXTERNAL_ES=1 (you almost certainly do not)." >&2
       exit 1
     fi ;;
esac

echo "Applying composable index templates for chain '$CHAIN' -> $ES${MODE:+ (index.mode=$MODE)}"
for t in action delta abi; do
  body="$(sed "s/{{CHAIN}}/$CHAIN/g" "$root/templates/$t.json")"
  [ -n "$MODE" ] && body="$(printf '%s' "$body" | sed "s/\"index\": {/\"index\": {\n          \"mode\": \"$MODE\",/")"
  code="$(printf '%s' "$body" | curl -fsS -o /tmp/ds-bench-tmpl.out -w '%{http_code}' \
            -XPUT "$ES/_index_template/${CHAIN}-${t}" -H 'Content-Type: application/json' --data-binary @-)" \
    && echo "  ${CHAIN}-${t}: HTTP $code $(cat /tmp/ds-bench-tmpl.out)" \
    || { echo "  ${CHAIN}-${t}: FAILED $(cat /tmp/ds-bench-tmpl.out)" >&2; exit 1; }
done
echo "Done. Templates: $(curl -fsS "$ES/_cat/templates/${CHAIN}-*?h=name" | tr '\n' ' ')"
