//! The injected script, assembled at compile time.
//!
//! The script is `js/webview-sockets.js` with the JS codec of the proto crate
//! inserted at the codec marker.

use webview_sockets_proto::{JS_CODEC, JS_CODEC_EXPORT_MARKER};

const HOST_JS: &str = include_str!("../js/webview-sockets.js");
const CODEC_MARKER: &str = "// @@webview-sockets-codec@@";

/// Returns the byte offset of `needle` in `haystack`.
///
/// # Panics
///
/// Panics if `needle` is missing. In a const context this fails the build.
const fn find(haystack: &str, needle: &str) -> usize {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    let mut start = 0;
    while start + needle.len() <= haystack.len() {
        let mut matched = 0;
        while matched < needle.len() && haystack[start + matched] == needle[matched] {
            matched += 1;
        }
        if matched == needle.len() {
            return start;
        }
        start += 1;
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

/// Returns the head of the host script, the codec and the tail of the host
/// script as one array.
const fn concat() -> [u8; LEN] {
    let mut out = [0u8; LEN];
    let parts = [HEAD, CODEC, TAIL];
    let mut pos = 0;
    let mut part_index = 0;
    while part_index < parts.len() {
        let part = parts[part_index];
        let mut byte_index = 0;
        while byte_index < part.len() {
            out[pos] = part[byte_index];
            pos += 1;
            byte_index += 1;
        }
        part_index += 1;
    }
    out
}

static BYTES: [u8; LEN] = concat();

/// The line of [`INJECTED_JS`] that [`injected_js_for`] replaces.
const HOSTS_LINE: &str = "const HOSTS = null; // @@webview-sockets-hosts@@";

/// The classic script to inject into the app's pages at document start.
///
/// In the top frame of http(s) pages it defines `window.webviewSockets`
/// with `version`, `udp.bind()`, `udp.connect()`, `UDPSocket` and
/// `requestSession()`. It also defines `window.UDPSocket`, a Direct Sockets
/// compatibility layer, unless the browser has a native one.
///
/// This variant runs on every host. See [`injected_js_for`] for one limited
/// to some hosts.
///
/// The script talks to the platform over `window.webviewSocketsBridge`
/// (Android web message listener) or `window.ipc.postMessage` (desktop,
/// replies through `window.__webviewSocketsReply(id, json)`). Requests are
/// JSON `{"ns": "webview-sockets", "id", "op"}`, see
/// [`crate::BridgeRequest`]. Pages without either bridge use
/// `window.__webviewSocketsDevSession()` if it is defined (dev only).
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
        assert!(INJECTED_JS.contains("udp = Object.freeze({ bind, connect })"));
        assert!(!INJECTED_JS.contains(CODEC_MARKER));
        assert!(!INJECTED_JS.contains("\nexport "));
        assert!(INJECTED_JS.is_ascii());
    }
}
