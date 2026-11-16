# webview-sockets

Real UDP sockets for web pages shown in a webview, provided by the app that
embeds the webview. Pages get a small, fast socket API
(`webviewSockets.udp`). For code written against the standard, there is
also a [Direct Sockets](https://wicg.github.io/direct-sockets/)
`UDPSocket` compatibility layer. Rust code compiled to wasm gets a
`std::net::UdpSocket` lookalike. The app owns the sockets and decides, per
origin and with a user prompt, who may use them.

```js
const sock = await webviewSockets.udp.bind({ localAddress: "::" }); // may prompt the user
sock.onmessage = ({ data, address, port }) => console.log(address, port, data);
sock.send("hello", "203.0.113.7", 4433);

const peer = await webviewSockets.udp.connect("example.org", 4433); // the app resolves the name
peer.send(new Uint8Array([1, 2, 3]));
```

- [Why](#why)
- [How it works](#how-it-works)
- [The page API](#the-page-api)
- [Security model](#security-model)
- [Embedding](#embedding)
- [Rust in the page](#rust-in-the-page)
- [Wire protocol](#wire-protocol)
- [Demos and tests](#demos-and-tests)
- [Status](#status)

## Why

Browsers do not give web pages raw sockets. Chrome's Direct Sockets API is
the standard way to fix that, but it only ships in Chrome's Isolated Web
Apps and ChromeOS. Android `WebView`, WebKitGTK, WKWebView and WebView2 do
not offer it, and an app cannot turn it on.

But an app that shows web pages in a webview often wants them to talk UDP:
peer-to-peer apps (QUIC, iroh, WebRTC-free hole punching), local network
tools, game clients, or a browser for a peer-to-peer web. The app itself can
open sockets, so the missing piece is a safe bridge from the page to the
app. webview-sockets is that bridge, and it gives pages the same API they
would use with Direct Sockets, so the same code runs in an Isolated Web App
and in a webview.

## How it works

```text
+----------------------------- app -------------------------------+
|                                                                 |
|  webview                                local HTTP server       |
|  +-----------------------------+        +---------------------+ |
|  | page http://app.localhost:N |  HTTP  | app pages           | |
|  |                             |<------>|                     | |
|  |  injected script:           |   WS   | /.well-known/       | |
|  |   webviewSockets.udp -------+------->|  webview-sockets/udp|--+--> UDP
|  |   (one WebSocket per socket)|        |  (UdpHost::router)  | |
|  |                             |        +---------------------+ |
|  |   requestSession() ---------+--> platform bridge (Android    |
|  |     <- { url, token }       |    web message listener, wry   |
|  +-----------------------------+    IPC) -> UdpHost::request    |
|                                          -> prompt -> answer    |
+-----------------------------------------------------------------+
```

1. The app serves its pages from a local HTTP server, on an origin such as
   `http://app.localhost:8080`, and merges `UdpHost::router()` into that
   server.
2. At document start the app injects a script
   (`UdpHost::injected_js()`) into its pages. It defines
   `window.webviewSockets`, and `UDPSocket` if the browser has none.
3. The first `webviewSockets.udp.bind(...)` (or `new UDPSocket(...)`) asks the app for a session over the platform
   bridge: an Android web message listener or wry's IPC. The platform
   reports the page's real origin, which the page cannot fake. The app
   checks its policy and grants, and may prompt the user. It answers with a
   one-tab session token.
4. Every socket is one WebSocket to `/.well-known/webview-sockets/udp` on
   the page's own origin. The first message carries the token and the
   socket options. The host checks `Origin` and `Host`, opens the UDP
   socket, and answers with the bound addresses. After that, datagrams flow
   both ways, several per WebSocket message.

Because the WebSocket is same-origin, no mixed content, CORS or
private-network rules get in the way, and it works with the stock webview
of every platform. The token proves the bridge granted access. The origin
check makes sure only the granted origin can use the token.

## The page API

The injected script defines `window.webviewSockets` in the top frame of
http(s) pages on the hosts the app allows:

- `udp.bind(options?)` -> `Promise<Socket>`: a bound socket. Options:
  `localAddress` (default `"0.0.0.0"`; `"::"` is dual-stack), `localPort`
  (default 0, any port), `ipv6Only`, `sendBufferSize`, `receiveBufferSize`,
  `gro`.
- `udp.connect(host, port, options?)` -> `Promise<Socket>`: a connected
  socket. `host` is an IP address or a name that the app resolves. It only
  talks to that peer. Options: `dnsQueryType` (`"ipv4"` or `"ipv6"`),
  `sendBufferSize`, `receiveBufferSize`, `gro`.
- `UDPSocket`: the Direct Sockets class, see below.
- `requestSession()` -> `Promise<{ url, token }>`: asks the app for a
  session, which may prompt the user. `bind`, `connect` and `UDPSocket` call
  it as needed. It is for code that speaks the wire protocol directly, such
  as the Rust crate, or to hand a session to a Worker.
- `version`: the wire protocol version (2).

The first socket asks for a session, so that is when the user sees the
prompt.

### Socket

| Member | |
| --- | --- |
| `send(data, address?, port?, { segmentSize?, ecn?, dnsQueryType? })` | Queues one datagram. `data` is a `Uint8Array`, `ArrayBuffer`, other `ArrayBufferView`, or a string (sent as UTF-8). `address` may be a host name, which the app resolves. On a connected socket, leave out `address` and `port`. All sends in one task go to the app in one WebSocket message. |
| `onmessage({ data, address, port, ecn, segmentSize? })` | One received datagram. `data` is a `Uint8Array` view into the received message. It stays valid, but copy it if you keep many. |
| `onerror({ name, code, message, error })` | A send failed in the app, for example a forbidden destination or a name that does not resolve. The socket stays open. Without a handler, the error is logged. |
| `onclose({ code, reason, error? })` | The socket closed. `error` is set (a `DOMException`) unless the page closed it: `NotAllowedError` when access was revoked or switched off, otherwise `NetworkError`. |
| `close()` | Sends what is queued and closes. |
| `localAddress`, `localPort` | The bound address. |
| `remoteAddress`, `remotePort` | The resolved peer of a connected socket, or `null`. |
| `interfaceAddrs` | `[{ address, port }]`: the local interface addresses with the bound port, to tell peers. |
| `maxGsoSegments`, `groSegments` | How many datagrams the app sends and receives per syscall (1 means no GSO/GRO). |
| `closed`, `bufferedAmount` | Whether the socket is closed, and the bytes not yet handed to the network (for backpressure). |

GSO: with `segmentSize`, `data` holds several datagrams of that size, and
the last may be shorter. The app sends them with as few syscalls as the OS
allows. Received GRO batches are split into single datagrams, unless the
socket was opened with `gro: true`. Then `onmessage` may get several
datagrams from one source at once, with `segmentSize` set.

`bind` and `connect` reject with a `DOMException`:

- `NotAllowedError` when the user or app denies access.
- `InvalidAccessError` when the app refuses the address, for example a
  privileged port or a local address not on this machine.
- `NetworkError` when the address is in use, a name does not resolve, a
  limit is exceeded, or the app cannot be reached.
- `NotSupportedError` when there is no bridge to an app.

They reject with a `TypeError` for malformed options.

### Direct Sockets `UDPSocket` (compatibility layer)

The script also installs `window.UDPSocket`, following the
[Direct Sockets spec](https://wicg.github.io/direct-sockets/), when the
browser has no native one. It is built on the socket above, with Streams.
Code written for Chrome's Isolated Web Apps runs unchanged. Code written
against it runs on Chrome's native implementation, as long as it skips the
extensions below.

It costs throughput. Streams add several promises and a copy per datagram,
so it reaches about half the packet rate of `webviewSockets.udp` (see
[Performance](#performance)). Use `webviewSockets.udp` where speed matters.

| Spec | Supported |
| --- | --- |
| `new UDPSocket(options)` | yes; option checks match the spec (`TypeError`) |
| connected mode: `remoteAddress` (name or IP), `remotePort`, `dnsQueryType` | yes; the app resolves the name |
| bound mode: `localAddress`, `localPort`, `ipv6Only` | yes |
| `sendBufferSize`, `receiveBufferSize` | yes, capped by the app |
| `opened` -> `{ readable, writable, localAddress, localPort, remoteAddress?, remotePort? }` | yes |
| `readable` of `{ data: Uint8Array, remoteAddress?, remotePort? }` | yes (remote fields in bound mode only) |
| `writable` of `{ data: BufferSource, remoteAddress?, remotePort?, dnsQueryType? }` | yes; bound mode may send to host names |
| `closed`, `close()` | yes, including the `InvalidStateError` rules |
| multicast options, `multicastController` | no: `NotAllowedError`, as for a page without the multicast permission |
| Workers | no, the script only runs in the top window |

Errors use the spec's names:

- `TypeError` for invalid options or messages.
- `NotAllowedError` when the user or app denies access, when access is
  revoked or switched off while a socket is open, and for multicast,
  broadcast, unspecified or port 0 destinations.
- `InvalidAccessError` when the app refuses the address.
- `NetworkError` when binding fails, a name does not resolve, a host limit
  is exceeded, or the connection to the app drops.
- `InvalidStateError` from `close()`, when the socket is not open yet or a
  stream is locked.

As with a native `UDPSocket`, a rejected write errors the `writable`. The
`readable` queues up to 4096 datagrams. When the page reads more slowly,
further datagrams are dropped, as a full socket buffer would.

These extensions are not part of Direct Sockets:

- `segmentSize` (GSO) and `ecn` on written messages. Read messages carry
  `ecn` when it is not 0.
- Constructor option `gro: true`: a read message may hold several
  datagrams, with `segmentSize` set.
- `opened` also has `interfaceAddresses`, `maxGsoSegments` and
  `groSegments`.
- `socket.onsenderror = (err) => ...`: a `DOMException` for a send that
  failed in the app after its write resolved. The socket stays open.

`webviewSockets.UDPSocket` is always the compatibility class, even when
`window.UDPSocket` is native.

### Performance

These numbers come from the demo page's benchmark, run in headless Chrome
against `udp-echo` on loopback. It sends 1200-byte datagrams, using a
release test-host on a loaded 22-core machine, so the spread is wide:

| | RTT p50 | Throughput, window 64 |
| --- | --- | --- |
| `webviewSockets.udp` | 0.4-0.6 ms | 17-29k pkt/s each way (160-280 Mbit/s) |
| `UDPSocket` layer | 0.3-0.8 ms | 7-14k pkt/s each way (70-140 Mbit/s) |

A page-only benchmark replaces the app with an in-page echo, so it
measures just the script's CPU cost. It gets 53-75k pkt/s with
`webviewSockets.udp` and 6-13k pkt/s with `UDPSocket`. Earlier runs of the
same benchmark on an idle machine reached 55-63k pkt/s for the fast API
(and 75-84k pkt/s for the Rust crate with GSO).

## Security model

- **Off by default.** The app turns sockets on (`UdpHost::set_enabled`).
  Turning them off closes every socket and voids every token.
- **Per origin, with consent.** The first request from an origin asks the
  user (the app shows the prompt). The choices are "allow always"
  (persisted), "allow once" (until the tab navigates or closes) and "block"
  (persisted). The app can list and revoke grants. Revoking closes the
  origin's sockets.
- **Only the app's origins.** A `HostPolicy` limits which hosts may use
  sockets at all. With `HostPolicy::patterns`, the injected script also
  returns early on other hosts, so foreign pages do not even see the API.
  On Android, also pass the same origins to `addDocumentStartJavaScript` and
  `addWebMessageListener`.
- **The origin comes from the platform, never from the page.** Bridge
  requests are answered for the origin that Android or wry reports. Only
  the top frame can ask, because the script does not run in iframes.
- **Tokens are bound to the origin and the tab.** A token is 32 random
  bytes. It works only on WebSockets whose `Origin` header matches the
  granted origin and whose `Host` is allowed. It dies when the tab
  navigates or closes. A page on another origin that learns the token cannot
  use it, because the browser sets `Origin`. Local native processes can
  forge headers, so for them the secret token is the barrier.
- **Destinations.** Pages cannot send multicast or broadcast, or to the
  unspecified address or port 0. They cannot bind privileged ports
  (below 1024) or multicast addresses. Loopback and private addresses are
  allowed. A page with a grant can reach the local network, and the prompt
  should say so.
- **Resource limits.** `Limits` caps sockets per tab (16 by default) and,
  as a safety cap, per origin across all tabs (64), plus message size,
  queued datagrams and kernel buffer sizes, and puts timeouts on the
  handshake and name resolution.

The page's own JavaScript is trusted with the sockets once the user allowed
its origin. That is the same trust model as Direct Sockets, so grant only
origins whose content the app controls or the user trusts.

## Embedding

The host crate is
[`webview-sockets-host`](host/README.md). An app needs four pieces:

1. A `UdpHost`, built with a grant store path and a `HostPolicy`. Merge
   `host.router()` into the axum server that serves the pages.
2. Inject `host.injected_js()` at document start.
3. Answer bridge requests (`{"ns":"webview-sockets","id","op":"requestSession"}`)
   with `UdpHost::request` / `UdpHost::answer` and `bridge_reply`.
4. Call `UdpHost::tab_navigated` and `UdpHost::tab_closed`.

```rust
use webview_sockets_host::{HostPolicy, UdpHost, UdpHostConfig};

let policy = HostPolicy::patterns(["*.app.localhost:8080"]);
let host = UdpHost::new(UdpHostConfig::new(data_dir.join("socket-grants.json"), policy))?;
host.set_enabled(true); // or from a settings switch
let app = axum::Router::new()
    .merge(host.router())
    .fallback_service(serve_pages);
```

### Android WebView (androidx.webkit)

Run the server and `UdpHost` in Rust (through JNI or UniFFI), and wire the
bridge with `androidx.webkit` (needs the `DOCUMENT_START_SCRIPT` and
`WEB_MESSAGE_LISTENER` features):

```kotlin
val rules = setOf("http://*.app.localhost:8080")
WebViewCompat.addDocumentStartJavaScript(webView, Native.injectedScript(), rules)
WebViewCompat.addWebMessageListener(webView, "webviewSocketsBridge", rules) {
        view, message, sourceOrigin, isMainFrame, reply ->
    if (!isMainFrame) return@addWebMessageListener
    val request = JSONObject(message.data ?: return@addWebMessageListener)
    val origin = sourceOrigin.toString().trimEnd('/') // from WebView, not from the page
    // Native.request: UdpHost::request(origin, tab) -> granted / prompt / denied / disabled.
    // On "prompt", show a dialog, then Native.answer(origin, tab, choice).
    // Reply with the JSON from bridge_reply(id, ...):
    reply.postMessage(Native.handle(origin, tab, request.getLong("id")))
}
```

The listener name must be `webviewSocketsBridge`. Call
`UdpHost::tab_navigated` from `WebViewClient.onPageStarted` (main frame) and
`UdpHost::tab_closed` when the tab goes away. WebView treats
`*.localhost` as loopback, so `http://<name>.localhost:<port>` pages count
as a secure context.

### wry (desktop)

```rust
use webview_sockets_host::{BridgeRequest, Decision, bridge_reply, reply_script};

let builder = WebViewBuilder::new()
    .with_initialization_script(&host.injected_js())
    .with_ipc_handler(move |request: http::Request<String>| {
        let Some(req) = BridgeRequest::parse(request.body()) else {
            return; // not ours: the app's own IPC messages
        };
        // The page's real origin, from the webview, not from the message.
        let origin = origin_of(request.uri()); // "http://app.localhost:8080"
        let reply = match host.request(&origin, tab) {
            Decision::Granted(ticket) => bridge_reply(req.id, Ok(&ticket)),
            Decision::NeedPrompt => return prompt_later(origin, req.id), // then host.answer(...)
            Decision::Denied => bridge_reply(req.id, Err("NotAllowed")),
            Decision::Disabled => bridge_reply(req.id, Err("Disabled")),
        };
        // Delivers only if the page is still on `origin`.
        webview.evaluate_script(&reply_script(&origin, req.id, &reply)).ok();
    });
```

The script uses `window.ipc.postMessage` when there is no Android bridge, and
takes the reply through `window.__webviewSocketsReply(id, json)`, which
`reply_script` calls. With `HostPolicy::patterns` the script stays inert on
other sites the webview may show.

### Without an app

The `test-host` example serves a directory and grants sessions over a dev
HTTP endpoint, so pages can be tried in a normal browser:

```sh
cargo run -p webview-sockets-host --example test-host -- --dir demos/js --auto-grant
# open http://test.localhost:<port>/
```

## Rust in the page

[`webview-sockets`](webview-sockets/README.md) is a `UdpSocket` shaped like
`std::net::UdpSocket`. On native targets it wraps the std socket. On
`wasm32-unknown-unknown` built with `--cfg webview_sockets`, it speaks the wire
protocol to the app. Without the cfg, every call fails with `Unsupported`.

```toml
[dependencies]
webview-sockets = { git = "ssh://git@github.com/Frando/webview-sockets.git", branch = "main" }
```

```sh
RUSTFLAGS='--cfg webview_sockets' cargo build --target wasm32-unknown-unknown
```

```rust
webview_sockets::init().await?; // requestSession(), may prompt
let sock = webview_sockets::UdpSocket::bind("[::]:0")?;
sock.bound().await?;
sock.send_to_async(b"hello", "203.0.113.7:4433".parse()?).await?;
let mut buf = [0u8; 1500];
let (n, from) = sock.recv_from_async(&mut buf).await?;
```

On wasm, sockets are non-blocking. The crate has poll and async helpers,
GSO/GRO batching (`send_to_segmented`, `recv_from_segmented`) and
`connect_host` for connected mode. The cfg `webview_sockets` is also what
the netwatch and iroh forks key on to run iroh over real UDP in a page.

## Wire protocol

This is defined in [`webview-sockets-proto`](proto/README.md), with the
same codec in Rust (`no_std`, zero-copy) and JavaScript, checked against
shared test vectors. One WebSocket carries one socket. Binary messages:

```text
page -> host  HELLO  version 2, token, buffer sizes, mode:
                     bound (local addr, ipv6Only) | connected (dns, host, port)
host -> page  BOUND  local addr, remote addr (connected), maxGsoSegments,
                     groSegments, interface addresses     | ERROR code, message
both          DATA   ecn, peer addr, segment size, payload (repeated per message)
page -> host  DATA_NAMED  same with a host name instead of an address (bound mode)
host -> page  ERROR  a send failed; the socket stays open
```

Messages are at most 256 KiB. The host closes with 4001 (revoked), 4002
(switched off), 4003 (tab navigated), 4004 (limit) or 4005 (I/O error).

## Demos and tests

| Path | What |
| --- | --- |
| [`demos/js`](demos/js) | Plain JS on `webviewSockets.udp`: bound socket with echo mode, a connected socket by host name, automation and a benchmark; `?api=udpsocket` runs it all on `UDPSocket` |
| [`demos/wasm`](demos/wasm) | The same in Rust on the `webview-sockets` crate |
| `udp-echo` | Native echo server and client: `cargo run -p webview-sockets-demo --bin udp-echo -- listen` |
| [`host/examples/test-page`](host/examples/test-page) | Self test of `webviewSockets.udp` and `UDPSocket` in a browser |

```sh
cargo test --workspace
node proto/js/codec.test.mjs           # JS codec against the test vectors
node host/js/polyfill.test.mjs         # UDPSocket option checks, install rules
demos/test-js.sh [--bench]             # JS demo in headless Chrome
demos/wasm/build.sh && demos/wasm/test.sh [--bench]
WASM_BINDGEN=<wasm-bindgen 0.2.128> webview-sockets/tests/node/run.sh
```

The browser tests need Node 22 or later and `google-chrome-stable` (or set
`CHROME`). The self test runs with:

```sh
cargo run -p webview-sockets-host --example test-host -- --dir host/examples/test-page --auto-grant --port 47941 &
node host/examples/test-page/run-chrome.mjs http://test.localhost:47941/
```

## Status

The project is experimental. It is used on Android WebView and on wry
(WebKitGTK) by the iroh-link browser. The crates are not on crates.io yet,
and the API may change.

Not supported:

- TCP (`TCPSocket`, `TCPServerSocket`).
- Multicast.
- Sockets in Workers. A Worker can use a session from `requestSession()`
  through the Rust crate's `init_with_session`, but `webviewSockets` itself is
  only defined in the top window.
- Per-send delivery errors. Sends report failures asynchronously
  (`onerror`, or `onsenderror` on `UDPSocket`, where the write has
  already resolved).
- `UDPSocket` is about half as fast as `webviewSockets.udp`.

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
