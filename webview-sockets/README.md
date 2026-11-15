# wasm-native-udp

A UDP socket shaped like `std::net::UdpSocket` that also works in web pages
opened in the iroh link apps. On native targets it wraps the std socket. On
`wasm32-unknown-unknown` with `--cfg wasm_native_udp`, the socket lives in the
app and the page talks to it over a WebSocket (see
[`iroh-link-udp-host`](../iroh-link-udp-host/README.md) and
[`iroh-link-udp-proto`](../iroh-link-udp-proto/README.md)). Without the cfg,
every call fails with `Unsupported`.

On wasm the socket behaves like a non-blocking std socket and offers
poll and async helpers to wait, plus GSO and GRO style batching. It is what
the forked `netwatch` uses to run iroh in a page, see the
[demos](../iroh-link-udp-demo/README.md).

```sh
cargo test -p wasm-native-udp
# The wasm backend, under Node against a mock host (Node >= 22 and the
# wasm-bindgen CLI matching the crate version):
WASM_BINDGEN=<path to wasm-bindgen> wasm-native-udp/tests/node/run.sh
```

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
