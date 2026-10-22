#!/usr/bin/env bash
# Read-only RPC latency benchmark for choosing chain.http_rpc / chain.preconf_http_rpc.
# Run it on the server the engine runs on (latency from a laptop says nothing).
#
#   ALCHEMY_KEY=… scripts/rpc-latency.sh            # default endpoints, 50 rounds
#   N=100 scripts/rpc-latency.sh https://a.example https://b.example
#
# Per endpoint: p50/p95/max ms and errors for a QuoterV2 eth_call (1 WETH → USDC, 0.05%)
# at `latest` and at `pending`, and whether `pending` is Flashblocks state (the pending
# block grows between two reads 300 ms apart). Only eth_call / eth_getBlockByNumber are sent — no keys,
# no transactions. URLs are printed with any key path redacted.
set -euo pipefail

N="${N:-50}"
QUOTER=0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a
QUOTE_DATA=0xc6a5026a0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda029130000000000000000000000000000000000000000000000000de0b6b3a764000000000000000000000000000000000000000000000000000000000000000001f40000000000000000000000000000000000000000000000000000000000000000

if [ "$#" -gt 0 ]; then
  URLS=("$@")
else
  URLS=("https://mainnet.base.org" "https://mainnet-preconf.base.org")
  [ -n "${ALCHEMY_KEY:-}" ] && URLS+=("https://base-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}")
fi

redact() { sed -E 's#(/v[0-9]+/)[^/?]+#\1***#; s#([?&](key|apikey|api_key|token)=)[^&]+#\1***#I' <<<"$1"; }

rpc() { # url body → prints "<ms> <response>"
  curl -s -m 5 -o /dev/stdout -w '\n%{time_total}' -H 'content-type: application/json' -d "$2" "$1" 2>/dev/null \
    | awk 'NR==1{body=$0} NR>1{t=$0} END{printf "%d %s\n", t*1000, body}'
}

stats() { # stdin: one ms value per line
  sort -n | awk '{a[NR]=$1} END{ if (NR==0) { print "n/a"; exit }
    p50=a[int((NR-1)*0.50)+1]; p95=a[int((NR-1)*0.95)+1];
    printf "p50 %4d ms  p95 %4d ms  max %4d ms", p50, p95, a[NR] }'
}

bench() { # url tag
  local url=$1 tag=$2 ok=() err=0 body out ms resp
  body="{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$QUOTER\",\"data\":\"$QUOTE_DATA\"},\"$tag\"]}"
  rpc "$url" "$body" >/dev/null || true # warm the connection
  for _ in $(seq "$N"); do
    out=$(rpc "$url" "$body" || echo "0 ")
    ms=${out%% *}; resp=${out#* }
    if grep -q '"result":"0x[0-9a-f]\{64\}' <<<"$resp"; then ok+=("$ms"); else err=$((err + 1)); fi
  done
  printf '  quote @%-8s %s  errors %d/%d\n' "$tag" "$(printf '%s\n' "${ok[@]+"${ok[@]}"}" | grep . | stats)" "$err" "$N"
}

pending_state() { # url → "<number> <tx count>"
  rpc "$1" '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["pending",false]}' | cut -d' ' -f2- \
    | python3 -c 'import json,sys; b=json.load(sys.stdin)["result"]; print(int(b["number"],16), len(b["transactions"]))' 2>/dev/null
}

for url in "${URLS[@]}"; do
  echo "$(redact "$url")"
  first=$(pending_state "$url" || true); sleep 0.3; second=$(pending_state "$url" || true)
  if [ -n "$first" ] && [ -n "$second" ] && [ "$first" != "$second" ]; then
    echo "  pending block: $first → $second (block, txs) — Flashblocks state"
  else
    echo "  pending block: ${first:-none} → ${second:-none} — not growing; don't use as preconf_http_rpc"
  fi
  bench "$url" latest
  bench "$url" pending
done
