//! The injected script, assembled at compile time from `js/webview-sockets.js` and
//! the proto crate's JS codec.

use webview_sockets_proto::{JS_CODEC, JS_CODEC_EXPORT_MARKER};

const HOST_JS: &str = include_str!("../js/webview-sockets.js");
const CODEC_MARKER: &str = "// @@webview-sockets-codec@@";

/// Returns the byte offset of `needle` in `haystack`; fails the build if
/// it is missing.
const fn find(haystack: &str, needle: &str) -> usize {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0;
    while i + n.len() <= h.len() {
        let mut j = 0;
        while j < n.len() && h[i + j] == n[j] {
            j += 1;
        }
        if j == n.len() {
            return i;
        }
        i += 1;
    }
    panic!("marker not found");
}

const HEAD: &[u8] = HOST_JS.as_bytes().split_at(find(HOST_JS, CODEC_MARKER)).0;
const TAIL: &[u8] = HOST_JS
    .as_bytes()
    .split_at(find(HOST_JS, CODEC_MARKER) + CODEC_MARKER.len())
    .1;
const CODEC: &[u8] = JS_CODEC
    .as_bytes()
    .split_at(find(JS_CODEC, JS_CODEC_EXPORT_MARKER))
    .0;
const LEN: usize = HEAD.len() + CODEC.len() + TAIL.len();

const fn concat() -> [u8; LEN] {
    let mut out = [0u8; LEN];
    let parts = [HEAD, CODEC, TAIL];
    let mut pos = 0;
    let mut p = 0;
    while p < parts.len() {
        let part = parts[p];
        let mut i = 0;
        while i < part.len() {
            out[pos] = part[i];
            pos += 1;
            i += 1;
        }
        p += 1;
    }
    out
}

static BYTES: [u8; LEN] = concat();

/// The classic script to inject into the app's pages at document start.
///
/// It defines `window.webviewSockets` (version 2) with `requestSession()`
/// and `udp.bind(opts)` in the top frame of http(s) pages. It talks to the
/// platform over `window.webviewSocketsBridge` (Android web message
/// listener) or `window.ipc.postMessage` (desktop, replies via
/// `window.__webviewSocketsReply(id, json)`); requests are JSON
/// `{"ns": "webview-sockets", "id", "op"}`, see [`crate::BridgeRequest`].
/// Pages without either bridge fall back to
/// `window.__webviewSocketsDevSession()` if defined (dev only).
pub const INJECTED_JS: &str = match core::str::from_utf8(&BYTES) {
    Ok(s) => s,
    Err(_) => panic!("injected script is not utf-8"),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembled() {
        assert!(INJECTED_JS.contains("const webviewSocketsCodec"));
        assert!(INJECTED_JS.contains("requestSession"));
        assert!(!INJECTED_JS.contains(CODEC_MARKER));
        assert!(!INJECTED_JS.contains("\nexport "));
        assert!(INJECTED_JS.is_ascii());
    }
}
