# iroh-link-udp-host

The host side of native UDP sockets for pages opened in the iroh link apps
([Android](../iroh-link-android/README.md) and
[desktop](../iroh-link-desktop/README.md)). A page asks
`window.irohHost.udp` for a session, the app asks the user, and then every
socket is one WebSocket to the gateway origin, speaking
[`iroh-link-udp-proto`](../iroh-link-udp-proto/README.md). Access is off by
default and granted per origin, once or always.

The crate provides the WebSocket route, the session and permission store, and
the script injected into pages ([`js/iroh-host.js`](js/iroh-host.js)). The
platform supplies the prompt and the bridge between the page and Rust.

To try pages without an app, the `test-host` example serves a directory with
a dev version of `window.irohHost`:

```sh
cargo run -p iroh-link-udp-host --example test-host -- --dir <dir> --auto-grant
```

Then open `http://test.pkarr.localhost:<port>/`.
[`examples/test-page/run-chrome.mjs`](examples/test-page/run-chrome.mjs) drives
such pages in headless Chrome. Tests: `cargo test -p iroh-link-udp-host`.

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
