#!/usr/bin/env bash
# End-to-end test of the plain-JS demo (js/) in headless Chrome against the
# webview-sockets-host test-host and udp-echo.
#
#   ./test-js.sh [--bench]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
host_port=47921
echo_port=47922
run="$repo/host/examples/test-page/run-chrome.mjs"

(cd "$repo" && cargo build -q -p webview-sockets-demo && cargo build -q -p webview-sockets-host --example test-host)
target_dir="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')"

"$target_dir/debug/udp-echo" listen --port "$echo_port" --bind :: >/dev/null &
pids=($!)
"$target_dir/debug/examples/test-host" --dir "$here/js" --port "$host_port" --auto-grant >/dev/null 2>&1 &
pids+=($!)
trap 'kill "${pids[@]}" 2>/dev/null' EXIT
sleep 1

base="http://test.localhost:$host_port/"
fail=0
check() { # want(PASS|FAIL) query [timeout_ms]
  local out
  out="$(node "$run" "$base?$2" UDP-DEMO "${3:-20000}" | grep '^title:' || true)"
  if [[ "$out" == "title: UDP-DEMO $1"* ]]; then echo "ok   $2 -> ${out#title: }"; else echo "FAIL $2 -> $out"; fail=1; fi
}
check PASS "peer=127.0.0.1:$echo_port&send=hello&expect=hello"
check PASS "peer=[::1]:$echo_port&send=v6&expect=v6"
check FAIL "peer=127.0.0.1:$echo_port&send=hello&expect=other"
check FAIL "peer=0.0.0.0:$echo_port&send=x"
check PASS "connect=localhost:$echo_port&send=named&expect=named&dnsQueryType=ipv4"
check PASS "connect=[::1]:$echo_port&send=c6&expect=c6"
check FAIL "connect=no-such-host.invalid:$echo_port&send=x"
if [ "${1:-}" = "--bench" ]; then
  check PASS "bench=127.0.0.1:$echo_port" 120000
fi
exit $fail
