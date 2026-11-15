#!/usr/bin/env bash
# Builds the node_test example for wasm with --cfg wasm_native_udp and runs it
# under Node against tests/node/mock-host.mjs.
#
# Needs: rustup target wasm32-unknown-unknown, Node >= 22, and wasm-bindgen
# CLI matching the wasm-bindgen crate version (set WASM_BINDGEN to override).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
build_dir="${CARGO_BUILD_BUILD_DIR:-$repo/target}"
out="${OUT_DIR:-/tmp/wasm-native-udp-node-test}"
wasm_bindgen="${WASM_BINDGEN:-wasm-bindgen}"

cd "$repo"
RUSTFLAGS='--cfg wasm_native_udp' cargo build -p wasm-native-udp \
  --example node_test --target wasm32-unknown-unknown
# Final artifacts go to the target dir, not the build dir.
target_dir="$(cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')"
wasm="$target_dir/wasm32-unknown-unknown/debug/examples/node_test.wasm"
[ -f "$wasm" ] || wasm="$build_dir/wasm32-unknown-unknown/debug/examples/node_test.wasm"
mkdir -p "$out"
"$wasm_bindgen" --target nodejs --out-dir "$out" "$wasm"
node "$here/mock-host.mjs" "$out/node_test.js"
