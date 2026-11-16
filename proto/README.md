# iroh-link-udp-proto

The wire format between a web page and the iroh link app for native UDP
sockets. One WebSocket on `/.well-known/iroh-link/udp` of the page's own origin
carries one UDP socket: the page sends `HELLO`, the host answers `BOUND` or
`ERROR`, then both sides exchange `DATA` messages holding one or more
datagrams. The crate is `no_std` and decodes without copying.

The same codec exists in JavaScript in [`js/codec.js`](js/codec.js), and both
implementations are checked against the shared test vectors in
[`testdata/vectors.json`](testdata/vectors.json):

```sh
cargo test -p iroh-link-udp-proto
node udp-proto/js/codec.test.mjs
```

The host side is [`iroh-link-udp-host`](../udp-host/README.md), the
Rust page side is [`wasm-native-udp`](../wasm-native-udp/README.md).

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
