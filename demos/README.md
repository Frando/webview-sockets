# webview-sockets-demo

Demo pages for webview-sockets. Serve them with the `test-host` example of
[`webview-sockets-host`](../host/README.md), or open them in an app that
embeds the host.

- [`js/`](js): a plain JavaScript page using `window.webviewSockets.udp`.
- [`wasm/`](wasm): the same in Rust, with
  [`webview-sockets`](../webview-sockets/README.md) compiled to wasm. It is
  its own Cargo workspace (excluded from the main one), since it builds for
  `wasm32-unknown-unknown` with `--cfg webview_sockets` (see
  `wasm/.cargo/config.toml`).
- `udp-echo`: a native echo server and client to talk to
  (`cargo run -p webview-sockets-demo --bin udp-echo -- listen`).

## Build and test

`wasm/` needs the `wasm32-unknown-unknown` target and the wasm-bindgen CLI
of the version pinned in its `Cargo.toml` (set `WASM_BINDGEN` to its path):

```sh
wasm/build.sh && wasm/test.sh
```

The test script opens the page in headless Chrome through the `test-host`
example, against `udp-echo`. Pages are driven by query parameters, listed at
the top of [`wasm/public/main.js`](wasm/public/main.js) and
[`js/main.js`](js/main.js).

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
