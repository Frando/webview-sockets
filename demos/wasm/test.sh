#!/usr/bin/env bash
# End-to-end test of the wasm demo in headless Chrome against the
# webview-sockets-host test-host and udp-echo. Run ./build.sh first.
#
#   ./test.sh [--bench]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
host_port=47931
echo_port=47932
run="$repo/host/examples/test-page/run-chrome.mjs"
[ -f "$here/public/wasm/webview_sockets_demo_wasm_bg.wasm" ] || { echo "run ./build.sh first" >&2; exit 1; }

(cd "$repo" && cargo build -q -p webview-sockets-demo && cargo build -q -p webview-sockets-host --example test-host)
target_dir="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')"
echo_bin="$target_dir/debug/udp-echo"
host_bin="$target_dir/debug/examples/test-host"

"$echo_bin" listen --port "$echo_port" --bind :: >/dev/null &
pids=($!)
"$host_bin" --dir "$here/public" --port "$host_port" --auto-grant >/dev/null 2>&1 &
pids+=($!)
trap 'kill "${pids[@]}" 2>/dev/null' EXIT
sleep 1

base="http://test.localhost:$host_port/"
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
