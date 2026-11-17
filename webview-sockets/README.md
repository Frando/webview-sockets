# webview-sockets

A UDP socket shaped like `std::net::UdpSocket` that also works in web pages
whose app runs [`webview-sockets-host`](../host/README.md).

- native: wraps `std::net::UdpSocket`.
- `wasm32-unknown-unknown` with `--cfg webview_sockets`: a socket in the
  host app over [`webview-sockets-proto`](../proto/README.md).
- wasm without the cfg: every call fails with `Unsupported`.

Libraries depend on it unconditionally and the final binary picks the
backend through the cfg, so a library such as netwatch builds everywhere
and gets real UDP where the app provides it.

## Usage

```toml
[dependencies]
webview-sockets = { git = "ssh://git@github.com/Frando/webview-sockets.git", branch = "main" }
```

```sh
RUSTFLAGS='--cfg webview_sockets' cargo build --target wasm32-unknown-unknown
```

```rust
webview_sockets::init().await?; // requestSession(), may prompt; no-op on native
let sock = webview_sockets::UdpSocket::bind("[::]:0")?;
sock.bound().await?; // wasm: wait until the app bound it
sock.send_to_async(b"hello", "203.0.113.7:4433".parse()?).await?;
let mut buf = [0u8; 1500];
let (len, from) = sock.recv_from_async(&mut buf).await?;

let peer = webview_sockets::UdpSocket::connect_host("example.org", 4433, None)?;
```

On wasm, sockets are non-blocking (`WouldBlock`; use `poll_*_ready` or the
`*_async` helpers), and sends before `bound()` are queued.
`send_to_segmented` / `recv_from_segmented` do GSO/GRO. Errors map to
`PermissionDenied` (denied or revoked), `ConnectionAborted`,
`InvalidInput`, `NotFound` (name not resolved) and `Unsupported` (no
`window.webviewSockets`). In a Worker, use `init_with_session(url, token)`
with a session from the page.

## Tests

```sh
cargo test -p webview-sockets
# wasm backend under Node against a mock host (Node 22+, wasm-bindgen 0.2.128):
WASM_BINDGEN=<path to wasm-bindgen> webview-sockets/tests/node/run.sh
```

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE)
or [MIT license](../LICENSE-MIT), at your option.
