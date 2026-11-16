# webview-sockets

A UDP socket shaped like `std::net::UdpSocket` that also works in web pages
whose webview app runs [`webview-sockets-host`](../host/README.md). On native
targets it wraps the std socket. On `wasm32-unknown-unknown` with
`--cfg webview_sockets`, the socket lives in the app and the page talks to
it over a WebSocket (see [`webview-sockets-proto`](../proto/README.md)).
Without the cfg, every call fails with `Unsupported`.

On wasm the socket behaves like a non-blocking std socket and offers
poll and async helpers to wait, plus GSO and GRO style batching.
`UdpSocket::connect_host` opens a socket connected to a host name that the
app resolves. A fork of `netwatch` uses this crate to run iroh in a page.

```sh
cargo test -p webview-sockets
# The wasm backend, under Node against a mock host (Node >= 22 and the
# wasm-bindgen CLI matching the pinned version, 0.2.128):
WASM_BINDGEN=<path to wasm-bindgen> webview-sockets/tests/node/run.sh
```

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
