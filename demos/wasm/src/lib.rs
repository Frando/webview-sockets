//! Native UDP demo for webview pages, with all networking in Rust.
//!
//! Built for `wasm32-unknown-unknown` with `--cfg webview_sockets`, the
//! [`webview_sockets::UdpSocket`] talks to the host app. `public/main.js`
//! only drives the UI and calls the exports of this crate:
//!
//! - `start()` asks for a session, binds `[::]:0` (falling back to
//!   `0.0.0.0:0`) and starts the receive loop. It resolves to a `Demo`.
//! - `Demo` has getters for the bound and interface addresses and the GSO and
//!   GRO limits, `send_text`, `send_gso`, `set_echo`, callbacks for log lines
//!   and socket close, and the automation runs `send_expect`, `gso_expect`
//!   and `bench`, which resolve to `"PASS ..."` or `"FAIL ..."`.
//!
//! On other targets this crate is empty.

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod app;
