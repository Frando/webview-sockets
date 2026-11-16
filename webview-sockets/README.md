# webview-sockets

A UDP socket shaped like `std::net::UdpSocket` that also works in web pages
whose webview app runs [`webview-sockets-host`](../host/README.md).

| Target | Backend |
| --- | --- |
| native | `std::net::UdpSocket` |
| `wasm32-unknown-unknown` with `--cfg webview_sockets` | a socket in the host app, over one WebSocket per socket ([`webview-sockets-proto`](../proto/README.md)) |
| `wasm32-unknown-unknown` without the cfg | every call fails with `io::ErrorKind::Unsupported` |

Libraries depend on it unconditionally. The final binary chooses the
backend through the cfg, so a library such as netwatch builds for every
target and gets real UDP only where the app provides it.

## Usage

```toml
[dependencies]
webview-sockets = { git = "ssh://git@github.com/Frando/webview-sockets.git", branch = "main" }
```

```sh
RUSTFLAGS='--cfg webview_sockets' cargo build --target wasm32-unknown-unknown
# or in .cargo/config.toml:
# [target.wasm32-unknown-unknown]
# rustflags = ["--cfg", "webview_sockets"]
```

```rust
use webview_sockets::UdpSocket;

// Asks the app for a session through window.webviewSockets.requestSession().
// This may show a permission prompt. On native it does nothing.
webview_sockets::init().await?;

let sock = UdpSocket::bind("[::]:0")?;
sock.bound().await?; // wasm: wait until the app bound the socket
sock.send_to_async(b"hello", "203.0.113.7:4433".parse()?).await?;
let mut buf = [0u8; 1500];
let (len, from) = sock.recv_from_async(&mut buf).await?;

// Connected mode: the app resolves the name.
let peer = UdpSocket::connect_host("example.org", 4433, None)?;
peer.bound().await?;
```

On wasm:

- Sockets are non-blocking. `send_to` and `recv_from` return `WouldBlock`.
  Wait with `poll_recv_ready` / `poll_send_ready`, or use the `*_async`
  helpers.
- `bind` returns at once. `bound().await` waits for the app's answer.
  Until then, sends are queued.
- `send_to_segmented` and `recv_from_segmented` batch datagrams (GSO/GRO).
  `max_gso_segments` and `gro_segments` report what the app supports.
- `interface_addrs` returns the app's interface addresses with the bound
  port.
- Errors map to `io::ErrorKind`: denied or revoked -> `PermissionDenied`, a
  dropped connection -> `ConnectionAborted`, a bad address ->
  `InvalidInput`, a name that does not resolve -> `NotFound`, no
  `window.webviewSockets` -> `Unsupported`.
- In a Worker, call `init_with_session(url, token)` with a session the page
  got from `webviewSockets.requestSession()`.

## Tests

```sh
cargo test -p webview-sockets
# The wasm backend under Node against a mock host (Node >= 22 and the
# wasm-bindgen CLI of the pinned version, 0.2.128):
WASM_BINDGEN=<path to wasm-bindgen> webview-sockets/tests/node/run.sh
```

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
