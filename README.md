# webview-sockets

Native sockets for web pages in a webview, provided by the app that embeds
the webview. Pages get UDP sockets shaped like the Direct Sockets
`UDPSocket` (bound and connected mode, host names resolved by the app),
Rust code compiled to wasm gets a `std::net::UdpSocket` lookalike. The app
serves its pages from a local HTTP server; each socket is one WebSocket to
the page's own origin, and the app decides per origin, with a user prompt,
who may use sockets.

| Crate | What |
| --- | --- |
| [`webview-sockets-proto`](proto/README.md) | Wire format (Rust and JavaScript codec, shared test vectors) |
| [`webview-sockets-host`](host/README.md) | Host side: WebSocket route, permissions, injected page script |
| [`webview-sockets`](webview-sockets/README.md) | `UdpSocket` for Rust, native and wasm (`--cfg webview_sockets`) |
| [`webview-sockets-demo`](demos/README.md) | Demo pages and the `udp-echo` CLI |

```sh
cargo test --workspace
node proto/js/codec.test.mjs
WASM_BINDGEN=<wasm-bindgen 0.2.128> webview-sockets/tests/node/run.sh
```

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
