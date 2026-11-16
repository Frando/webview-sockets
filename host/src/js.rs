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

/// The line of [`INJECTED_JS`] that [`injected_js_for`] replaces.
const HOSTS_LINE: &str = "const HOSTS = null; // @@webview-sockets-hosts@@";

/// The classic script to inject into the app's pages at document start.
///
/// In the top frame of http(s) pages it defines `window.UDPSocket`, the
/// Direct Sockets `UDPSocket` (unless the browser has a native one), and
/// `window.webviewSockets` with `version`, `UDPSocket` and
/// `requestSession()`. This variant runs on every host; see
/// [`injected_js_for`] for one limited to some hosts. It talks to the
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

/// Returns [`INJECTED_JS`] limited to pages whose host matches a pattern.
///
/// Patterns are host authorities where `*` matches one or more characters
/// other than `.` and `:`, as in [`crate::HostPolicy::patterns`]. On other
/// hosts the script returns without defining anything.
pub fn injected_js_for<S: AsRef<str>>(patterns: &[S]) -> String {
    let list: Vec<&str> = patterns.iter().map(AsRef::as_ref).collect();
    let json = serde_json::to_string(&list).expect("serializable");
    INJECTED_JS.replacen(HOSTS_LINE, &format!("const HOSTS = {json};"), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gated() {
        assert!(INJECTED_JS.contains(HOSTS_LINE));
        let js = injected_js_for(&["*.localhost:*", "a\"b"]);
        assert!(!js.contains(HOSTS_LINE));
        assert!(js.contains(r#"const HOSTS = ["*.localhost:*","a\"b"];"#));
    }

    #[test]
    fn assembled() {
        assert!(INJECTED_JS.contains("const webviewSocketsCodec"));
        assert!(INJECTED_JS.contains("requestSession"));
        assert!(INJECTED_JS.contains("class UDPSocket"));
        assert!(!INJECTED_JS.contains(CODEC_MARKER));
        assert!(!INJECTED_JS.contains("\nexport "));
        assert!(INJECTED_JS.is_ascii());
    }
}
