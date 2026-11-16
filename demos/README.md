# webview-sockets-demo

Demo pages for webview-sockets. They run in an app that embeds
[`webview-sockets-host`](../host/README.md), or in a normal browser through
the host's `test-host` example.

- [`js/`](js): plain JavaScript on `webviewSockets.udp`. It binds a
  socket (`udp.bind({ localAddress: "::" })`), shows the bound and interface
  addresses, sends to a peer, and has an echo mode. It also opens connected
  sockets by host name (`udp.connect(host, port, { dnsQueryType })`). With
  `?api=udpsocket`, the same page runs on the Direct Sockets `UDPSocket`
  layer, which is useful for comparing the two.
- [`wasm/`](wasm): the same in Rust, with
  [`webview-sockets`](../webview-sockets/README.md) compiled to wasm. It has
  its own Cargo workspace, which the main one excludes, because it builds
  for `wasm32-unknown-unknown` with `--cfg webview_sockets` (see
  `wasm/.cargo/config.toml`).
- `udp-echo`: a native echo server and client to talk to:
  `cargo run -p webview-sockets-demo --bin udp-echo -- listen --port 4433`,
  and `udp-echo send <addr> <text>`.

## Automation

Both pages take query parameters and report `UDP-DEMO PASS|FAIL <detail>`
in the console and the page title:

- `?peer=ip:port&send=text&expect=text`: sends, waits for the reply and
  compares it.
- `?connect=host:port&send=text&expect=text[&dnsQueryType=ipv4]` (JS):
  does the same through a connected socket. The host may be a name.
- `?peer=ip:port&gso=4` (wasm): sends 4 datagrams in one segmented send and
  waits for all the echoes.
- `?bench=ip:port[&n=2000&count=20000&window=64]`: runs a 1200-byte
  ping-pong (RTT percentiles), then a windowed throughput test against an
  echo server. In the wasm demo, `&gso=1` sends each window refill as one
  segmented send.
- `?echo=1`: starts in echo mode.
- `?api=udpsocket` (JS): uses `new UDPSocket(...)` for all of the above.

## Build and test

```sh
./test-js.sh [--bench]               # JS demo in headless Chrome vs udp-echo
WASM_BINDGEN=<wasm-bindgen 0.2.128> wasm/build.sh
wasm/test.sh [--bench]               # wasm demo, same checks
```

`wasm/` needs the `wasm32-unknown-unknown` target and the wasm-bindgen CLI
of the version pinned in its `Cargo.toml`. The test scripts need Node 22 or
later and `google-chrome-stable` (or set `CHROME`).

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
