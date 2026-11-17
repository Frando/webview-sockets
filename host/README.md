# webview-sockets-host

The app side of webview-sockets: gives pages in your webview UDP sockets
backed by real sockets in your app. See the [project README](../README.md)
for the page API and the security model.

- `UdpHost`: sessions, per-origin grants (persisted to a JSON file),
  "allow once" grants per tab, limits, and an active-tabs watch for a
  "using sockets" indicator.
- `UdpHost::router()`: the axum route on `/.well-known/webview-sockets/udp`,
  to merge into the server that serves your pages.
- `UdpHost::injected_js()`: the script to inject at document start, gated
  to the policy's hosts (`INJECTED_JS` is ungated). Source:
  [`js/webview-sockets.js`](js/webview-sockets.js) plus the codec from
  `webview-sockets-proto`.
- `BridgeRequest`, `bridge_reply`, `reply_script`: the bridge messages.

The app provides the HTTP server, the bridge, the permission prompt, and
the tab lifecycle calls (`tab_navigated` when the main frame navigates,
`tab_closed`).

## Usage

```rust
use webview_sockets_host::{
    BridgeRequest, Choice, Decision, HostPolicy, TabId, UdpHost, UdpHostConfig, bridge_reply,
};

let host = UdpHost::new(UdpHostConfig::new(
    data_dir.join("socket-grants.json"),
    HostPolicy::patterns(["*.app.localhost:8080"]),
))?;
host.set_enabled(true);
let router = axum::Router::new().merge(host.router()); // plus your pages
let script = host.injected_js(); // inject at document start

// A bridge message from a page. `origin` must come from the platform.
fn on_bridge(host: &UdpHost, origin: &str, tab: TabId, json: &str) -> Option<String> {
    let req = BridgeRequest::parse(json)?;
    Some(match host.request(origin, tab) {
        Decision::Granted(ticket) => bridge_reply(req.id, Ok(&ticket)),
        Decision::NeedPrompt => match host.answer(origin, tab, Choice::AllowOnce) {
            // ask the user first
            Some(ticket) => bridge_reply(req.id, Ok(&ticket)),
            None => bridge_reply(req.id, Err("NotAllowed")),
        },
        Decision::Denied => bridge_reply(req.id, Err("NotAllowed")),
        Decision::Disabled => bridge_reply(req.id, Err("Disabled")),
    })
}
```

`HostPolicy::patterns` takes host authorities where `*` matches one label
or port (`*.localhost:*`). It is checked on the bridge origin and on the
WebSocket `Host`, and `Origin` must match `Host`. `HostPolicy::from_fn`
takes any rule. `HostPolicy::Any` is for dev tools.

### Platform bridges

- **Android (androidx.webkit):** inject with `addDocumentStartJavaScript`
  and register `addWebMessageListener` under the name
  `webviewSocketsBridge`, both with the same origin rules as the policy.
  Ignore messages where `isMainFrame` is false, take the origin from
  `sourceOrigin` (trim the trailing `/`), and reply with `bridge_reply`.
  Call `tab_navigated` from `WebViewClient.onPageStarted`.
- **wry:** inject with `with_initialization_script`. In the IPC handler,
  `BridgeRequest::parse` the body (other messages are the app's own), take
  the origin from the request URI, and deliver the reply with
  `webview.evaluate_script(&reply_script(&origin, id, &reply))`, which only
  reaches the page if it is still on that origin.

## Testing pages without an app

The `test-host` example serves a directory for `*.localhost:*` hosts, with
the script at `/.well-known/webview-sockets/polyfill.js` and a dev bridge
over HTTP. Without `--auto-grant` every request is denied, to test the
`NotAllowedError` paths.

```sh
cargo run -p webview-sockets-host --example test-host -- --dir <dir> --auto-grant [--port N]
cargo test -p webview-sockets-host
node host/js/polyfill.test.mjs
node host/examples/test-page/run-chrome.mjs http://test.localhost:<port>/   # ?denied=1 for the denied path
```

[`examples/test-page`](examples/test-page) is a self test of both APIs.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE)
or [MIT license](../LICENSE-MIT), at your option.
