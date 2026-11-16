# iroh-link-udp-demo

Demo pages for native UDP in the iroh link apps
([Android](../android/README.md),
[desktop](../desktop/README.md)). Publish a directory with
`publish-dir` and open its link in an app, or serve it locally with the
`test-host` example of [`iroh-link-udp-host`](../udp-host/README.md).
This directory is its own Cargo workspace, excluded from the main one, and so
are `wasm/` and `iroh/`, since they build for `wasm32-unknown-unknown` with
their own cfgs (see their `.cargo/config.toml`).

- [`js/`](js): a plain JavaScript page using `window.irohHost.udp`.
- [`wasm/`](wasm): the same in Rust, with
  [`wasm-native-udp`](../wasm-native-udp/README.md) compiled to wasm.
- [`iroh/`](iroh): a full iroh endpoint in the page, using a fork of iroh and
  netwatch that sends UDP through `wasm-native-udp`. It speaks the protocol of
  iroh's `transfer` example, so it can ping, download from and upload to the
  `transfer` CLI over a direct path, in both directions.
- `udp-echo`: a native echo server and client to talk to
  (`cargo run --bin udp-echo -- listen`).

## Build and test

`wasm/` and `iroh/` need the `wasm32-unknown-unknown` target and the
wasm-bindgen CLI of the version pinned in their `Cargo.toml` (set
`WASM_BINDGEN` to its path):

```sh
wasm/build.sh && wasm/test.sh
iroh/build.sh && iroh/test.sh
```

The test scripts open the pages in headless Chrome through the `test-host`
example. `wasm/test.sh` runs against `udp-echo`, `iroh/test.sh` against the
`transfer` example of the iroh fork (`IROH_DIR`) and needs network access to
the n0 production relay and DNS servers.

The iroh demo only uses the n0 production servers. The `transfer` CLI defaults
to staging, so pass `--env prod` after the subcommand:

```sh
cargo run --release -p iroh --example transfer -- provide --env prod
cargo run --release -p iroh --example transfer -- fetch <endpoint id> --env prod --mode ping
```

Pages are driven by query parameters, listed at the top of
[`wasm/public/main.js`](wasm/public/main.js), [`js/main.js`](js/main.js) and
[`iroh/src/app.rs`](iroh/src/app.rs). For the iroh demo:
`?connect=<endpoint id>&mode=ping|transfer|duration&size=100M&duration=10`,
`?listen` to accept connections, and `&require_direct=1` to fail without a
direct path.

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
