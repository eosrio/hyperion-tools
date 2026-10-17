#!/usr/bin/env bash
# Capture nodeos's /v1/chain block endpoint answers, byte for byte, as goldens for the chain
# API renderer and server (crates/core/testdata/chain-api) and as input for the nodeos
# differential test (crates/ship-archive/tests/chain_api_nodeos.rs).
#
# Usage:
#   capture_chain_api_goldens.sh --url http://ADDRESS:PORT --out DIR [--blocks-dir DIR] [--block N]...
#   capture_chain_api_goldens.sh --image IMAGE --out DIR [--block N]...
#   capture_chain_api_goldens.sh --bodies --url URL --out DIR --block N...
#
# Raw mode (--url, or --image) sends each request over a plain TCP connection (bash's
# /dev/tcp) and keeps the answer exactly as it arrived: status line, headers in their order,
# body. It writes
#   DIR/requests/<case>.req, .resp   block endpoint requests: the error matrix (G-ERR: every
#                                    error body and the request forms nodeos reads), and for
#                                    each --block N get_block_info, get_block_header in both
#                                    modes and get_raw_block by number, and the last two by id
#   DIR/http/<case>.req, .resp       what the HTTP layer answers whatever the chain holds:
#                                    404s, OPTIONS, illegal targets, a missing Host, methods,
#                                    and the Connection and HTTP/1.0 variants of an Unknown Block
#   DIR/blocks/<N>.hex               each --block N as stored (a packed signed_block), read from
#                                    the block logs in --blocks-dir (blocks.log and retained
#                                    blocks-<first>-<last>.log, versions 1 to 3)
#   DIR/PROVENANCE                   the node's server_version_string and chain_id, the date
#
#   --url URL      A Spring 1.1+ node you run, with chain_api_plugin, over plain HTTP, started
#                  with `http-validate-host = false`: every request says `Host: 127.0.0.1`, so
#                  the files hold no address. Point it only at a node you run: raw answers keep
#                  whatever headers the server, or a proxy in front of it, adds.
#   --image IMAGE  Start a throwaway dev chain from IMAGE, any image with Spring's nodeos on its
#                  PATH (for example ubuntu:22.04 with the antelope-spring .deb installed), on a
#                  loopback port; capture once its block log holds every --block (default 1, 2
#                  and 3); stop it cleanly and read those blocks from its block log.
#
# Bodies mode (--bodies) asks any node, over HTTP or HTTPS (curl), for the four answers of each
# --block N and keeps the bodies only, so no header of the server or its proxies is recorded:
#   DIR/<N>/get_block_info.json, get_block_header.json, get_block_header-extensions.json,
#   get_raw_block.json, and DIR/PROVENANCE.
#
# No output file names the address asked, the image, or any local path. Requirements: bash 4,
# coreutils (od, head, tail, timeout), sed; curl for --bodies; docker for --image.
#
# Bodies nodeos reads differently from strict JSON parsers are left out: fractions and exponents,
# which fc turns into doubles or strings, and bare words, which it reads as strings (`tru` is
# "tru"). A server of archived ranges answers them as unparsable, a documented deviation.

set -euo pipefail
export LC_ALL=C
# Git Bash would rewrite the container paths given to docker.
export MSYS_NO_PATHCONV=1

die() { echo "capture_chain_api_goldens: $*" >&2; exit 1; }
usage() { sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit "${1:-0}"; }

URL="" IMAGE="" OUT="" BLOCKS_DIR="" BODIES=0
BLOCKS=()
while [ $# -gt 0 ]; do
  case $1 in
    --url) URL=$2; shift 2 ;;
    --image) IMAGE=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --blocks-dir) BLOCKS_DIR=$2; shift 2 ;;
    --block) BLOCKS+=("$2"); shift 2 ;;
    --bodies) BODIES=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 2 >&2 ;;
  esac
done
[ -n "$OUT" ] || die "--out is required"
[ -n "$URL" ] || [ -n "$IMAGE" ] || die "one of --url and --image is required"
[ -z "$URL" ] || [ -z "$IMAGE" ] || die "--url and --image exclude each other"
for n in ${BLOCKS[@]+"${BLOCKS[@]}"}; do
  case $n in ''|*[!0-9]*) die "--block takes a block number, not '$n'" ;; esac
done

json_field() { # FILE KEY: a string or number member of a flat JSON object
  sed -n "s/.*\"$2\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}[,}].*/\1/p" "$1" | head -n 1
}

provenance() { # INFO_JSON MODE
  {
    echo "server_version_string $(json_field "$1" server_version_string)"
    echo "chain_id $(json_field "$1" chain_id)"
    echo "captured $(date -u +%Y-%m-%d)"
    echo "mode $2"
    echo "by scripts/capture_chain_api_goldens.sh"
  } > "$OUT/PROVENANCE"
}

# ---- bodies mode -------------------------------------------------------------------------

if [ "$BODIES" = 1 ]; then
  [ -n "$URL" ] || die "--bodies needs --url"
  [ ${#BLOCKS[@]} -gt 0 ] || die "--bodies needs at least one --block"
  mkdir -p "$OUT"
  ask() { # PATH BODY FILE
    local code
    code=$(curl -sS -m 60 -X POST "$URL$1" --data-binary "$2" -o "$3" -w '%{http_code}') \
      || die "$1 failed"
    [ "$code" = 200 ] || die "$1 $2 answered $code"
  }
  ask /v1/chain/get_info "" "$OUT/.info.json"
  for n in "${BLOCKS[@]}"; do
    mkdir -p "$OUT/$n"
    ask /v1/chain/get_block_info "{\"block_num\":$n}" "$OUT/$n/get_block_info.json"
    ask /v1/chain/get_block_header "{\"block_num_or_id\":$n,\"include_extensions\":false}" \
      "$OUT/$n/get_block_header.json"
    ask /v1/chain/get_block_header "{\"block_num_or_id\":$n,\"include_extensions\":true}" \
      "$OUT/$n/get_block_header-extensions.json"
    ask /v1/chain/get_raw_block "{\"block_num_or_id\":$n}" "$OUT/$n/get_raw_block.json"
  done
  provenance "$OUT/.info.json" bodies
  rm -f "$OUT/.info.json"
  echo "captured the bodies of ${#BLOCKS[@]} blocks into $OUT"
  exit 0
fi

# ---- the block logs ----------------------------------------------------------------------

u32_at() { od -An -tu4 -j "$2" -N4 "$1" | tr -d ' \n'; }   # little-endian hosts
u64_at() { od -An -tu8 -j "$2" -N8 "$1" | tr -d ' \n'; }
size_of() { wc -c < "$1" | tr -d ' '; }

block_hex() { # DIR N: block N as stored, in lowercase hex
  local dir=$1 n=$2 log index version first count pos next len prev
  for log in "$dir"/blocks.log "$dir"/blocks-*.log; do
    [ -f "$log" ] || continue
    index=${log%.log}.index
    [ -f "$index" ] || continue
    version=$(u32_at "$log" 0)
    case $version in 1) first=1 ;; 2|3) first=$(u32_at "$log" 4) ;; *) continue ;; esac
    count=$(( $(size_of "$index") / 8 ))
    (( n >= first && n < first + count )) || continue
    pos=$(u64_at "$index" $(( (n - first) * 8 )))
    if (( n + 1 < first + count )); then
      next=$(u64_at "$index" $(( (n + 1 - first) * 8 )))
    else
      next=$(size_of "$log")
    fi
    # Each block is followed by its own position, as a u64.
    len=$(( next - 8 - pos ))
    [ "$(u64_at "$log" $(( pos + len )))" = "$pos" ] || die "$log: block $n is not where its index says"
    # `previous` (after timestamp, producer and confirmed) starts with block n - 1, big-endian.
    prev=$(od -An -tx1 -j $(( pos + 14 )) -N4 "$log" | tr -d ' \n')
    [ $(( 16#$prev )) = $(( n - 1 )) ] || die "$log: the block stored as $n follows $(( 16#$prev ))"
    tail -c +$(( pos + 1 )) "$log" | head -c "$len" | od -An -v -tx1 | tr -d ' \n'
    return 0
  done
  return 1
}

# ---- raw HTTP ----------------------------------------------------------------------------

HOST="" PORT="" CASES=0
set_address() { # URL
  local hostport=${1#http://}
  hostport=${hostport%%/*}
  case $1 in http://*) ;; *) die "raw mode takes a plain http:// URL" ;; esac
  HOST=${hostport%:*}
  PORT=${hostport##*:}
  [ "$PORT" != "$hostport" ] || PORT=80
}

# exchange REQUEST_FILE RESPONSE_FILE: send the request, keep one answer (two when the first
# is an interim 1xx), read by its Content-Length, which stays in BODY_LEN.
BODY_LEN=0
exchange() {
  local line len status
  exec 3<>"/dev/tcp/$HOST/$PORT" || die "cannot connect to the node"
  cat "$1" >&3
  : > "$2"
  while :; do
    len=""
    IFS= read -r -t 30 status <&3 || die "no answer to $(basename "$1")"
    printf '%s\n' "$status" >> "$2"
    while IFS= read -r -t 30 line <&3; do
      printf '%s\n' "$line" >> "$2"
      [ "$line" = $'\r' ] && break
      case ${line,,} in content-length:*) len=${line#*:}; len=${len//[!0-9]/} ;; esac
    done
    case $status in "HTTP/1."?" 1"??" "*) continue ;; esac
    break
  done
  BODY_LEN=${len:-0}
  if [ -n "$len" ]; then
    [ "$len" = 0 ] || timeout 30 head -c "$len" <&3 >> "$2"
  else
    timeout 30 cat <&3 >> "$2" || true
  fi
  exec 3<&- 3>&-
}

# request DIR CASE METHOD TARGET BODY_FORMAT [HEADER_LINE]...: the request bytes of one case,
# its body the printf expansion of BODY_FORMAT ("-" for no body and no Content-Length). Header
# lines replace the default Host line when one of them starts with "Host" or is "-host".
request() {
  local dir=$1 name=$2 method=$3 target=$4 format=$5 version=HTTP/1.1 h host="Host: 127.0.0.1"
  shift 5
  local extra=()
  for h in "$@"; do
    case $h in
      HTTP/1.0) version=$h ;;
      -host) host="" ;;
      Host:*) host=$h ;;
      *) extra+=("$h") ;;
    esac
  done
  local body=$OUT/.body
  if [ "$format" = - ]; then : > "$body"; else printf -- "$format" > "$body"; fi
  {
    printf '%s %s %s\r\n' "$method" "$target" "$version"
    [ -z "$host" ] || printf '%s\r\n' "$host"
    for h in ${extra[@]+"${extra[@]}"}; do printf '%s\r\n' "$h"; done
    case " ${extra[*]-} " in
      *Transfer-Encoding:*) ;;
      *) [ "$format" = - ] || printf 'Content-Length: %s\r\n' "$(size_of "$body")" ;;
    esac
    printf '\r\n'
    cat "$body"
  } > "$OUT/$dir/$name.req"
  exchange "$OUT/$dir/$name.req" "$OUT/$dir/$name.resp"
  CASES=$((CASES + 1))
}

RAW=/v1/chain/get_raw_block HDR=/v1/chain/get_block_header INFO=/v1/chain/get_block_info

# The error matrix. A value is the printf format of a JSON value.
error_matrix() {
  local id2=$1 name v
  # nodeos's empty-body rule: spaces only are trimmed; `{}` with spaces and TABs is empty.
  for v in empty:'' spaces:'   ' braces:'{}' braces-ws:'{ \t }' braces-lf:'{}\n' \
           tab-braces:'\t{}' lf:'\n' braces-sp:' {} '; do
    request requests "raw-body-${v%%:*}" POST $RAW "${v#*:}"
    request requests "info-body-${v%%:*}" POST $INFO "${v#*:}"
  done
  request requests raw-body-none-get GET $RAW -
  request requests hdr-body-empty POST $HDR ''

  # Each error once per endpoint and mode: they differ in source line and method.
  for v in len-empty:'""' len-65:"\"$(printf '%065d' 1)\"" id-zz:'"zz"' ub-max:'4294967295' \
           parse:'[1]'; do
    name=${v%%:*} v=${v#*:}
    request requests "raw-$name" POST $RAW "{\"block_num_or_id\":$v}"
    request requests "hdr0-$name" POST $HDR "{\"block_num_or_id\":$v,\"include_extensions\":false}"
    request requests "hdr1-$name" POST $HDR "{\"block_num_or_id\":$v,\"include_extensions\":true}"
  done
  request requests info-ub-max POST $INFO '{"block_num":4294967295}'
  request requests info-parse POST $INFO '{"block_num":"x"}'

  # block_num_or_id as fc converts it and get_raw_block resolves it.
  local wrong2=${id2:0:62}$([ "${id2:62:2}" = 00 ] && echo 01 || echo 00)
  for v in num-1:1 str-1:'"1"' plus-1:'"+1"' lead0:'"0001"' wrap:'"4294967297"' \
           minus-1-str:'"-1"' minus-1:-1 minus-0:-0 zero:'"0"' 2p32:'"4294967296"' \
           u64max:'"18446744073709551615"' u64over:'"18446744073709551616"' \
           u64over-num:18446744073709551616 space-1:'" 1"' 1-space:'"1 "' true:true \
           false:false null:null esc-u0031:'"\\u0031"' esc-b:'"\\b"' esc-f:'"\\f"' \
           esc-quote:'"a\\"b"' esc-backslash:'"a\\\\b"' raw-01:'"a\001b"' raw-7f:'"a\177b"' \
           raw-tab:'"a\tb"' invalid-utf8:'"\377\3761"' utf8:'"\303\2511"' odd-hex:'"abc"' \
           hex-0x:'"0x10"' dash:'"-"' plus:'"+"' len65-hex:"\"$(printf 'a%.0s' {1..65})\"" \
           zeros-64:"\"$(printf '%064d' 0)\"" id-2:"\"$id2\"" id-2-upper:"\"${id2^^}\"" \
           id-2-wrong:"\"$wrong2\"" short-id-1:'"00000001"' short-id-2:"\"${id2:0:20}\"" \
           obj:'{}' arr:'[1]' exp-str:'"1e5"'; do
    request requests "raw-v-${v%%:*}" POST $RAW "{\"block_num_or_id\":${v#*:}}"
  done

  # block_num as fc converts it: a 32-bit number.
  for v in num-1:1 str-1:'"1"' plus-1:'"+1"' minus-1:-1 minus-1-str:'"-1"' wrap:4294967297 \
           true:true false:false null:null x:'"x"' space-1:'" 1"' empty:'""' arr:'[1]' \
           obj:'{}' exp-str:'"1e5"'; do
    request requests "info-v-${v%%:*}" POST $INFO "{\"block_num\":${v#*:}}"
  done

  # Bodies as fc's parser reads them, and include_extensions as fc converts it.
  for v in missing-key:'{"x":1}' dup-first-wins:'{"block_num_or_id":"1","block_num_or_id":"2"}' \
           no-commas:'{"x":0 "block_num_or_id":"1"}' rep-commas:'{,,"block_num_or_id":"1",,}' \
           trailing-bytes:'{"block_num_or_id":"1"}garbage' \
           trailing-obj:'{"block_num_or_id":"1"}{"block_num_or_id":"2"}' \
           top-array:'[1]' top-string:'"x"' top-number:'1' bad-token:'nul' unterminated:'{bad' \
           unterminated-str:'{"block_num_or_id":"1' lead-ws:' \r\n\t{"block_num_or_id":"1"}' \
           nested:'{"a":{"b":[1,-2,"s",true,false,null,{},[]]},"block_num_or_id":"1"}' \
           key-escaped:'{"block_num_or_i\\d":"1"}' raw-04:'{"block_num_or_id":"\004"}'; do
    request requests "raw-s-${v%%:*}" POST $RAW "${v#*:}"
  done
  request requests raw-s-depth-150 POST $RAW \
    "{\"a\":$(printf '[%.0s' {1..150})$(printf ']%.0s' {1..150}),\"block_num_or_id\":\"1\"}"
  request requests raw-s-depth-250 POST $RAW \
    "{\"a\":$(printf '[%.0s' {1..250})$(printf ']%.0s' {1..250}),\"block_num_or_id\":\"1\"}"
  for v in 1:1 0:0 2:2 minus:-3 str-true:'"true"' str-false:'"false"' null:null \
           yes:'"yes"' empty:'""' arr:'[]'; do
    request requests "hdr-incl-${v%%:*}" POST $HDR \
      "{\"block_num_or_id\":\"1\",\"include_extensions\":${v#*:}}"
  done
}

# What the HTTP layer answers, whatever the chain holds: block 4294967295 is never served.
http_matrix() {
  local ub='{"block_num_or_id":4294967295}'
  request http ub POST $RAW "$ub"
  request http ub-close POST $RAW "$ub" "Connection: close"
  request http ub-keepalive POST $RAW "$ub" "Connection: keep-alive"
  request http ub-http10 POST $RAW "$ub" HTTP/1.0
  request http ub-http10-keepalive POST $RAW "$ub" HTTP/1.0 "Connection: keep-alive"
  request http ub-header POST $HDR "$ub"
  request http ub-info POST $INFO '{"block_num":4294967295}'
  for method in GET PUT DELETE PATCH FOO; do
    request http "ub-method-${method,,}" "$method" $RAW "$ub"
  done
  request http ub-chunked POST $RAW "1e\r\n$ub\r\n0\r\n\r\n" "Transfer-Encoding: chunked"
  request http ub-content-type POST $RAW "$ub" "Content-Type: text/plain"
  request http ub-origin POST $RAW "$ub" "Origin: http://10.0.0.1"
  request http ub-other-host POST $RAW "$ub" "Host: 10.0.0.1:1"
  request http no-host POST $RAW "$ub" -host
  request http empty-host POST $RAW "$ub" "Host:"
  request http no-host-options OPTIONS /v1/chain/nope - -host
  request http no-host-http10 POST $RAW "$ub" HTTP/1.0 -host
  for t in nope:/v1/chain/nope query:"$RAW?x=1" slash:"$RAW/" case:/v1/chain/Get_Raw_Block \
           double-slash:"/$RAW" encoded:/v1/chain/get%5Fraw%5Fblock root:/; do
    request http "404-${t%%:*}" POST "${t#*:}" "$ub"
  done
  request http 404-close POST /v1/chain/nope '{}' "Connection: close"
  request http 404-http10-keepalive POST /v1/chain/nope '{}' HTTP/1.0 "Connection: keep-alive"
  request http options OPTIONS $RAW -
  request http options-nope OPTIONS /v1/chain/nope -
  request http options-body OPTIONS $RAW "$ub"
  request http options-close OPTIONS $RAW - "Connection: close"
  request http target-asterisk OPTIONS '*' -
  request http target-absolute GET http://127.0.0.1/v1/chain/get_info -
  request http target-absolute-post POST "http://127.0.0.1$RAW" "$ub"
  request http target-dotdot POST "/v1/chain/..$RAW" "$ub"
  request http target-dotdot-end POST "$RAW/.." "$ub"
  request http target-dotdot-query POST "$RAW?a=.." "$ub"
  request http target-authority CONNECT 127.0.0.1:80 -
  request http target-dotdot-no-host POST "/v1/chain/..$RAW" "$ub" -host
}

# Each --block by number in every mode, and by id where the id is the key.
block_cases() { # N
  local n=$1 id
  request requests "block-$n-info" POST $INFO "{\"block_num\":$n}"
  request requests "block-$n-header" POST $HDR "{\"block_num_or_id\":$n,\"include_extensions\":false}"
  request requests "block-$n-header-ext" POST $HDR "{\"block_num_or_id\":$n,\"include_extensions\":true}"
  request requests "block-$n-raw" POST $RAW "{\"block_num_or_id\":$n}"
  id=$(block_id "$n")
  request requests "block-$n-header-ext-id" POST $HDR "{\"block_num_or_id\":\"$id\",\"include_extensions\":true}"
  request requests "block-$n-raw-id" POST $RAW "{\"block_num_or_id\":\"$id\"}"
}

block_id() { # N: block N's id, as get_block_info gives it
  request requests ".id" POST $INFO "{\"block_num\":$1}"
  CASES=$((CASES - 1))
  sed -n 's/.*"id":"\([0-9a-f]\{64\}\)".*/\1/p' "$OUT/requests/.id.resp"
}

get_info() { # FILE: get_info's body
  request requests ".info" POST /v1/chain/get_info ''
  CASES=$((CASES - 1))
  tail -c "$BODY_LEN" "$OUT/requests/.info.resp" > "$1"
}

capture() {
  CASES=0
  mkdir -p "$OUT/requests" "$OUT/http"
  get_info "$OUT/.info.json"
  grep -q '"chain_id"' "$OUT/.info.json" \
    || die "get_info failed (is http-validate-host false?): $(head -c 300 "$OUT/.info.json")"
  local id2
  id2=$(block_id 2)
  [ ${#id2} = 64 ] || die "no id for block 2"
  error_matrix "$id2"
  http_matrix
  for n in ${BLOCKS[@]+"${BLOCKS[@]}"}; do block_cases "$n"; done
  rm -f "$OUT/requests/.id.req" "$OUT/requests/.id.resp" \
    "$OUT/requests/.info.req" "$OUT/requests/.info.resp" "$OUT/.body"
}

save_blocks() { # DIR
  [ ${#BLOCKS[@]} -gt 0 ] || return 0
  mkdir -p "$OUT/blocks"
  for n in "${BLOCKS[@]}"; do
    block_hex "$1" "$n" > "$OUT/blocks/$n.hex" || die "block $n is not in the block logs of $1"
  done
}

lib_of() { json_field "$1" last_irreversible_block_num; }

mkdir -p "$OUT"
if [ -n "$URL" ]; then
  set_address "$URL"
  capture
  provenance "$OUT/.info.json" node
  [ -z "$BLOCKS_DIR" ] || save_blocks "$BLOCKS_DIR"
else
  command -v docker > /dev/null || die "--image needs docker"
  [ ${#BLOCKS[@]} -gt 0 ] || BLOCKS=(1 2 3)
  name=chain-api-goldens-$$
  logs=$OUT/.blocks-from-dev-chain
  cleanup() { docker rm -f "$name" > /dev/null 2>&1 || true; rm -rf "$logs"; }
  trap cleanup EXIT
  docker run -d --name "$name" -p 127.0.0.1::8888 --entrypoint nodeos "$IMAGE" \
    --data-dir /goldens/data --config-dir /goldens/config -e -p eosio \
    --plugin eosio::producer_plugin --plugin eosio::chain_api_plugin \
    --http-server-address 0.0.0.0:8888 --http-validate-host false > /dev/null
  published=$(docker port "$name" 8888/tcp | head -n 1)
  set_address "http://127.0.0.1:${published##*:}"
  max=0
  for n in "${BLOCKS[@]}"; do (( n > max )) && max=$n; done
  mkdir -p "$OUT/requests"
  lib=0
  for _ in $(seq 1 120); do
    # In a subshell: until nodeos listens, the exchange fails and exits that shell only.
    if (get_info "$OUT/.info.json") 2> /dev/null; then
      lib=$(lib_of "$OUT/.info.json")
      (( ${lib:-0} > max )) && break
    fi
    sleep 1
  done
  (( ${lib:-0} > max )) || die "the dev chain never made block $max irreversible"
  capture
  provenance "$OUT/.info.json" dev-chain
  # A clean stop writes the block log out; then read the blocks from it.
  docker stop -t 60 "$name" > /dev/null
  rm -rf "$logs"
  mkdir -p "$logs"
  hostpath=$logs
  command -v cygpath > /dev/null && hostpath=$(cygpath -w "$logs")
  docker cp "$name:/goldens/data/blocks/." "$hostpath" > /dev/null
  save_blocks "$logs"
fi
rm -f "$OUT/.info.json"
echo "captured $CASES requests into $OUT${BLOCKS[0]+ (blocks: ${BLOCKS[*]})}"
