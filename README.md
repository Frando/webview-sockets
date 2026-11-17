# webview-sockets

Optional UDP sockets for web pages in webviews, granted per site by the app
that hosts the webview. Sockets are off unless the app enables them and the
user allows the site. The app owns the real sockets. Pages get a small, fast
API (`webviewSockets.udp`) and a
[Direct Sockets](https://wicg.github.io/direct-sockets/) `UDPSocket`
compatibility layer. Rust code compiled to wasm gets a `std::net::UdpSocket`
lookalike.

```js
const sock = await webviewSockets.udp.bind({ localAddress: "::" }); // may prompt the user
sock.onmessage = ({ data, address, port }) => console.log(address, port, data);
sock.send("hello", "203.0.113.7", 4433);

const peer = await webviewSockets.udp.connect("example.org", 4433); // the app resolves the name
peer.send(new Uint8Array([1, 2, 3]));
```

Direct Sockets only ships in Chrome's Isolated Web Apps and ChromeOS.
Android `WebView`, WebKitGTK, WKWebView and WebView2 do not offer it, but the
app around them can open sockets. webview-sockets is the bridge, with the
same API, so the same code runs in both places.

## Components

- [`webview-sockets-host`](host/README.md) (`host/`): the app side. Grants,
  the WebSocket route, the injected script, the bridge messages.
- [`webview-sockets`](webview-sockets/README.md): `UdpSocket` for Rust,
  native or wasm in a page.
- [`webview-sockets-proto`](proto/README.md) (`proto/`): the wire format, in
  Rust and JavaScript.
- [`demos`](demos/README.md): JS and wasm demo pages, and the `udp-echo`
  server.

Used by the [iroh-link](https://github.com/Frando/iroh-link) browser on
Android and desktop. Builds:
[rolling release](https://github.com/Frando/iroh-link/releases/tag/rolling).

## How it works

1. The app serves its pages from a local HTTP server (for example
   `http://app.localhost:8080`) and merges `UdpHost::router()` into it.
2. The app injects `UdpHost::injected_js()` at document start. It defines
   `window.webviewSockets`, and `UDPSocket` if the browser has none.
3. The first socket asks the app for a session over the platform bridge (an
   Android web message listener or wry IPC). The platform reports the
   page's origin, so the page cannot fake it. The app may prompt the user
   and answers with a session token for that tab.
4. Each socket is one WebSocket to `/.well-known/webview-sockets/udp` on the
   page's own origin. The host checks the token, `Origin` and `Host`, opens
   the UDP socket, and datagrams flow both ways, several per message.

Because the WebSocket is same-origin, mixed content, CORS and
private-network rules do not apply, and it works in the stock webview of
every platform.

## The page API

- `webviewSockets.udp.bind(options?)`: a bound socket. Options
  `localAddress` (default `"0.0.0.0"`, `"::"` is dual-stack), `localPort`,
  `ipv6Only`, `sendBufferSize`, `receiveBufferSize`, `gro`.
- `webviewSockets.udp.connect(host, port, options?)`: a socket that only
  talks to one peer. `host` may be a name, which the app resolves
  (`dnsQueryType`: `"ipv4"` or `"ipv6"`).
- `webviewSockets.requestSession()`: the raw session `{ url, token }`, for
  code that speaks the wire protocol itself (the Rust crate, Workers).

A socket has `send(data, address?, port?, { segmentSize?, ecn? })`,
`onmessage({ data, address, port, ecn, segmentSize? })`, `onerror`
(a send failed, the socket stays open), `onclose({ code, reason, error? })`,
`close()`, the local and remote address, `interfaceAddrs`, and
`bufferedAmount` for backpressure. `segmentSize` sends a GSO batch. With
`gro: true`, `onmessage` may get a GRO batch.

`bind` and `connect` reject with `NotAllowedError` (denied),
`InvalidAccessError` (address refused), `NetworkError` (in use, no such
name, limit, app unreachable) or `NotSupportedError` (no app bridge).

`window.UDPSocket` follows the Direct Sockets spec, including its error
names, and is installed only when the browser has none
(`webviewSockets.UDPSocket` is always the compatibility class). It has no
multicast and does not run in Workers. Its Streams cost about half the
packet rate of `webviewSockets.udp` (roughly 7-14k vs 17-29k pkt/s on
loopback). Extensions beyond the spec: `segmentSize` and `ecn` on
messages, the `gro` option, `interfaceAddresses`, and `onsenderror`.

## Security model

- **Off by default.** `UdpHost::set_enabled` turns sockets on. Turning them
  off closes every socket and voids every token.
- **Per origin, with consent.** The user chooses "allow always", "allow
  once" (until the tab navigates or closes) or "block". The app can list
  and revoke grants. Revoking closes the origin's sockets.
- **Only the app's origins.** `HostPolicy` limits which hosts may use
  sockets. With `HostPolicy::patterns` the script does not even define the
  API on other hosts. On Android, pass the same origins to
  `addDocumentStartJavaScript` and `addWebMessageListener`.
- **The origin comes from the platform**, never from the page. Only the top
  frame can ask.
- **Tokens** are 32 random bytes, bound to the granted origin (checked
  against `Origin` and `Host`) and the tab. Local native processes can
  forge headers, so against them the secret token is the barrier.
- **Destinations.** No multicast, broadcast, unspecified address or port 0,
  and no binding below port 1024. Loopback and private addresses are
  allowed, so a granted page can reach the local network. The prompt should
  say so.
- **Limits** on sockets per tab (16) and per origin (64), message size,
  queues, buffer sizes, and handshake and DNS timeouts.

Once the user allows an origin, its JavaScript is trusted with the sockets,
as with Direct Sockets. Grant only origins the app or the user trusts.

## Quick start

```sh
cargo test --workspace
# try a page in a normal browser, without an app:
cargo run -p webview-sockets-host --example test-host -- --dir demos/js --auto-grant
# open http://test.localhost:<port>/
```

See [host/README.md](host/README.md) for embedding in an app and
[demos/README.md](demos/README.md) for the browser tests.

## Status

Experimental. Used on Android WebView and wry (WebKitGTK). Not on crates.io
yet, and the API may change. No TCP, no multicast, and `webviewSockets` is
only defined in the top window.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
