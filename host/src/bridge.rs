//! JSON messages on the platform bridge (Android web message listener, wry IPC).

use serde::{Deserialize, Serialize};

use crate::SessionTicket;

/// A request from the injected script:
/// `{"ns": "webview-sockets", "id": 1, "op": "requestSession"}`.
///
/// The `ns` field lets an app share one IPC channel (e.g. wry's
/// `window.ipc.postMessage`) between webview-sockets and its own messages.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BridgeRequest {
    /// Request id, echoed in the reply.
    pub id: u64,
    /// Operation; only `"requestSession"` exists.
    pub op: String,
}

/// A bridge message with its namespace, as sent by the injected script.
#[derive(Deserialize)]
struct Envelope {
    ns: String,
    #[serde(flatten)]
    request: BridgeRequest,
}

impl BridgeRequest {
    /// Value of the `ns` field of every bridge request.
    pub const NAMESPACE: &'static str = "webview-sockets";

    /// Asks for a session ticket.
    pub const OP_REQUEST_SESSION: &'static str = "requestSession";

    /// Parses a bridge message.
    ///
    /// Returns `None` if it is malformed or not in the webview-sockets
    /// namespace, so apps can try their own message types next.
    pub fn parse(json: &str) -> Option<Self> {
        let envelope: Envelope = serde_json::from_str(json).ok()?;
        (envelope.ns == Self::NAMESPACE).then_some(envelope.request)
    }
}

/// The JSON reply to a [`BridgeRequest`].
#[derive(Serialize)]
struct Reply<'a> {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// Builds the JSON reply for a bridge request.
///
/// `Ok` carries the ticket. `Err` carries a short reason shown to the page,
/// e.g. `"NotAllowed"`.
pub fn bridge_reply(id: u64, result: Result<&SessionTicket, &str>) -> String {
    let reply = match result {
        Ok(ticket) => Reply {
            id,
            ok: true,
            url: Some(&ticket.url),
            token: Some(&ticket.token),
            error: None,
        },
        Err(error) => Reply {
            id,
            ok: false,
            url: None,
            token: None,
            error: Some(error),
        },
    };
    serde_json::to_string(&reply).expect("serializable")
}

/// Returns a script that delivers a bridge reply to a desktop page.
///
/// Use it for hosts whose bridge has no reply channel, such as wry's
/// `window.ipc`, and run it with `evaluate_script`. It only delivers if the
/// page is still on `origin`, so a page that navigated in the meantime never
/// sees the token.
pub fn reply_script(origin: &str, id: u64, reply_json: &str) -> String {
    let origin = serde_json::to_string(origin).expect("serializable");
    let json = serde_json::to_string(reply_json).expect("serializable");
    format!(
        "if (location.origin === {origin} && window.__webviewSocketsReply) \
         window.__webviewSocketsReply({id}, {json});"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let req = BridgeRequest::parse(r#"{"ns":"webview-sockets","id":7,"op":"requestSession"}"#)
            .unwrap();
        assert_eq!(req.id, 7);
        assert_eq!(req.op, BridgeRequest::OP_REQUEST_SESSION);
        assert!(BridgeRequest::parse("{}").is_none());
        assert!(BridgeRequest::parse(r#"{"id":7,"op":"requestSession"}"#).is_none());
        assert!(BridgeRequest::parse(r#"{"ns":"app","id":7,"op":"requestSession"}"#).is_none());
        let ticket = SessionTicket {
            url: "ws://a/x".into(),
            token: "ab".into(),
        };
        assert_eq!(
            bridge_reply(7, Ok(&ticket)),
            r#"{"id":7,"ok":true,"url":"ws://a/x","token":"ab"}"#
        );
        assert_eq!(
            bridge_reply(8, Err("NotAllowed")),
            r#"{"id":8,"ok":false,"error":"NotAllowed"}"#
        );
    }

    #[test]
    fn reply_scripts() {
        let script = reply_script("http://a.localhost:5", 3, r#"{"id":3,"ok":false}"#);
        assert_eq!(
            script,
            r#"if (location.origin === "http://a.localhost:5" && window.__webviewSocketsReply) window.__webviewSocketsReply(3, "{\"id\":3,\"ok\":false}");"#
        );
    }
}
