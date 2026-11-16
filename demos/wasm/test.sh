#!/usr/bin/env bash
# End-to-end test of the wasm demo in headless Chrome against the
# iroh-link-udp-host test-host and udp-echo. Run ./build.sh first.
#
#   ./test.sh [--bench]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
host_port=47931
echo_port=47932
run="$repo/udp-host/examples/test-page/run-chrome.mjs"
[ -f "$here/public/wasm/iroh_link_udp_demo_wasm_bg.wasm" ] || { echo "run ./build.sh first" >&2; exit 1; }

(cd "$repo/iroh-link-udp-demo" && cargo build -q)
(cd "$repo" && cargo build -q -p iroh-link-udp-host --example test-host)
echo_bin="$(cd "$repo/iroh-link-udp-demo" && cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')/debug/udp-echo"
host_bin="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')/debug/examples/test-host"

"$echo_bin" listen --port "$echo_port" --bind :: >/dev/null &
pids=($!)
"$host_bin" --dir "$here/public" --port "$host_port" --auto-grant >/dev/null 2>&1 &
pids+=($!)
trap 'kill "${pids[@]}" 2>/dev/null' EXIT
sleep 1

base="http://test.pkarr.localhost:$host_port/"
fail=0
check() { # want(PASS|FAIL) query
  local out
  out="$(node "$run" "$base?$2" UDP-DEMO "${3:-20000}" | grep '^title:' || true)"
  if [[ "$out" == "title: UDP-DEMO $1"* ]]; then echo "ok   $2 -> ${out#title: }"; else echo "FAIL $2 -> $out"; fail=1; fi
}
check PASS "peer=127.0.0.1:$echo_port&send=hello&expect=hello"
check PASS "peer=[::1]:$echo_port&send=v6&expect=v6"
check FAIL "peer=127.0.0.1:$echo_port&send=hello&expect=other"
check PASS "peer=127.0.0.1:$echo_port&gso=4"
check FAIL "peer=0.0.0.0:$echo_port&send=x"
if [ "${1:-}" = "--bench" ]; then
  check PASS "bench=127.0.0.1:$echo_port" 120000
  check PASS "bench=127.0.0.1:$echo_port&gso=1" 120000
fi
exit $fail
