# webview-sockets-demo

Demo pages for webview-sockets. They run in an app that embeds
[`webview-sockets-host`](../host/README.md), or in a normal browser through
its `test-host` example.

- [`js/`](js): plain JavaScript on `webviewSockets.udp`: bound socket with
  echo mode, connected sockets by host name. `?api=udpsocket` runs the same
  on `UDPSocket`.
- [`wasm/`](wasm): the same in Rust with
  [`webview-sockets`](../webview-sockets/README.md). Its own Cargo workspace,
  built with `--cfg webview_sockets` (see `wasm/.cargo/config.toml`).
- `udp-echo`: native echo server and client:
  `cargo run -p webview-sockets-demo --bin udp-echo -- listen --port 4433`,
  `udp-echo send <addr> <text>`.

The pages report `UDP-DEMO PASS|FAIL <detail>` in the console and title for
query parameters like `?peer=ip:port&send=x&expect=x`,
`?connect=host:port&...` (JS), `?gso=4` (wasm), `?bench=ip:port` and
`?echo=1`.

## Build and test

```sh
./test-js.sh [--bench]               # JS demo in headless Chrome vs udp-echo
WASM_BINDGEN=<wasm-bindgen 0.2.128> wasm/build.sh
wasm/test.sh [--bench]
```

Needs the `wasm32-unknown-unknown` target, the pinned wasm-bindgen CLI,
Node 22+ and `google-chrome-stable` (or set `CHROME`).

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE)
or [MIT license](../LICENSE-MIT), at your option.
