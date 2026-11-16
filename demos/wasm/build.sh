#!/usr/bin/env bash
# Builds the wasm demo into public/wasm/.
#
#   ./build.sh [--debug]
#
# Needs the wasm32-unknown-unknown target and the wasm-bindgen CLI of the
# version pinned in Cargo.toml (set WASM_BINDGEN to its path if the one on
# PATH differs). The cfg webview_sockets comes from .cargo/config.toml.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"
profile=release
flag=--release
if [ "${1:-}" = "--debug" ]; then
  profile=debug
  flag=
fi
want="$(sed -n 's/^wasm-bindgen = "=\(.*\)"/\1/p' Cargo.toml)"
wasm_bindgen="${WASM_BINDGEN:-wasm-bindgen}"
have="$("$wasm_bindgen" --version | awk '{print $2}')"
if [ "$have" != "$want" ]; then
  echo "wasm-bindgen CLI is $have, need $want:" >&2
  echo "  cargo install wasm-bindgen-cli --version $want --root /some/dir && WASM_BINDGEN=/some/dir/bin/wasm-bindgen $0" >&2
  exit 1
fi
cargo build $flag --target wasm32-unknown-unknown
target_dir="$(cargo metadata --format-version 1 --no-deps | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')"
"$wasm_bindgen" --target web --no-typescript --out-dir public/wasm \
  "$target_dir/wasm32-unknown-unknown/$profile/webview_sockets_demo_wasm.wasm"
ls -l public/wasm
