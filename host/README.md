# webview-sockets-host

The app side of webview-sockets. It gives pages in your webview the Direct
Sockets `UDPSocket`, backed by real sockets in your app. See the
[project README](../README.md) for the design and the security model.

The crate provides:

- `UdpHost`: sessions, per-origin grants (persisted to a JSON file), "allow
  once" grants per tab, limits, and the active-tabs watch for a "using
  sockets" indicator.
- `UdpHost::router()`: the axum route on
  `/.well-known/webview-sockets/udp`, to merge into the server that serves
  your pages.
- `UdpHost::injected_js()`: the script to inject at document start. With
  `HostPolicy::patterns` it only runs on matching hosts. `INJECTED_JS` is
  the ungated version, and `injected_js_for(patterns)` gates by explicit
  patterns. The source is [`js/webview-sockets.js`](js/webview-sockets.js)
  plus the codec from `webview-sockets-proto`.
- `BridgeRequest`, `bridge_reply` and `reply_script`: the JSON messages on
  the platform bridge.

The app provides the HTTP server, the bridge (an Android web message
listener named `webviewSocketsBridge`, or wry's `window.ipc`), the
permission prompt, and the tab lifecycle calls.

## Usage

```rust
use webview_sockets_host::{
    BridgeRequest, Choice, Decision, HostPolicy, TabId, UdpHost, UdpHostConfig, bridge_reply,
};

// Pages on http://<name>.app.localhost:8080 may ask for sockets.
let host = UdpHost::new(UdpHostConfig::new(
    data_dir.join("socket-grants.json"),
    HostPolicy::patterns(["*.app.localhost:8080"]),
))?;
host.set_enabled(true);
let router = axum::Router::new().merge(host.router()); // plus your pages
let script = host.injected_js(); // inject at document start

// A bridge message from a page.  comes from the platform.
fn on_bridge(host: &UdpHost, origin: &str, tab: TabId, json: &str) -> Option<String> {
    let req = BridgeRequest::parse(json)?;
    Some(match host.request(origin, tab) {
        Decision::Granted(ticket) => bridge_reply(req.id, Ok(&ticket)),
        Decision::NeedPrompt => {
            // Ask the user, then:
            match host.answer(origin, tab, Choice::AllowOnce) {
                Some(ticket) => bridge_reply(req.id, Ok(&ticket)),
                None => bridge_reply(req.id, Err("NotAllowed")),
            }
        }
        Decision::Denied => bridge_reply(req.id, Err("NotAllowed")),
        Decision::Disabled => bridge_reply(req.id, Err("Disabled")),
    })
}
```

Call `host.tab_navigated(tab)` when a tab's main frame navigates, and
`host.tab_closed(tab)` when it closes. The project README has complete
Android (androidx.webkit) and wry examples.

`HostPolicy` decides which page hosts may use sockets. The check runs on
the bridge-reported origin and on the WebSocket's `Host`, and `Origin`
must match `Host`. `HostPolicy::patterns` takes host authorities where
`*` matches one label or port (`*.localhost:*`). `HostPolicy::from_fn`
takes any rule, and `HostPolicy::Any` is for dev tools.

## The injected script

In the top frame of http(s) pages on an allowed host, it defines:

- `window.UDPSocket`, the Direct Sockets API, if the browser has none.
- `window.webviewSockets`, which has `{ version, UDPSocket, requestSession() }`.

The project README lists the supported options, the error names and the
non-standard extensions (GSO/GRO `segmentSize`, `ecn`, interface addresses,
`onsenderror`).

## Testing pages without an app

The `test-host` example serves a directory on `127.0.0.1` for
`*.localhost:*` hosts. It serves the script at
`/.well-known/webview-sockets/polyfill.js` with a dev bridge that grants
sessions over HTTP. Pages load that script when `window.webviewSockets`
is missing.

```sh
cargo run -p webview-sockets-host --example test-host -- --dir <dir> --auto-grant [--port N]
# open http://test.localhost:<port>/
```

Without `--auto-grant` every request is denied, which tests the
`NotAllowedError` paths. [`examples/test-page`](examples/test-page) is a
self test of the API, and
[`run-chrome.mjs`](examples/test-page/run-chrome.mjs) drives such pages in
headless Chrome (`?denied=1` checks the denied path):

```sh
cargo test -p webview-sockets-host
node host/js/polyfill.test.mjs
node host/examples/test-page/run-chrome.mjs http://test.localhost:<port>/
```

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
