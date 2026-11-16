# webview-sockets-host

The host side of webview-sockets, for apps that show web pages in a webview
and serve them from a local HTTP server (the iroh-link Android and desktop
browsers are one user). A page calls `window.webviewSockets.requestSession()`,
the app asks the user, and then every socket is one WebSocket to the page's
own origin, speaking [`webview-sockets-proto`](../proto/README.md). Access
is off by default and granted per origin, once or always.

The crate provides the WebSocket route, the session and permission store, and
the script injected into pages ([`js/webview-sockets.js`](js/webview-sockets.js)).
The app supplies a `HostPolicy` (which page hosts may use sockets), the
prompt, and the bridge between the page and Rust: Android uses a web message
listener named `webviewSocketsBridge`, desktop wry's `window.ipc` with
JSON `{"ns": "webview-sockets", "id", "op"}` requests.

Sockets open in bound mode (a local address) or connected mode (a remote
host name or address that the host resolves, with an optional `ipv4` or
`ipv6` query type), like the Direct Sockets `UDPSocket`. Bound sockets
can also send to host names.

To try pages without an app, the `test-host` example serves a directory with
a dev version of `window.webviewSockets` at
`/.well-known/webview-sockets/polyfill.js`:

```sh
cargo run -p webview-sockets-host --example test-host -- --dir <dir> --auto-grant
```

Then open `http://test.localhost:<port>/`.
[`examples/test-page/run-chrome.mjs`](examples/test-page/run-chrome.mjs) drives
such pages in headless Chrome. Tests: `cargo test -p webview-sockets-host`.

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
