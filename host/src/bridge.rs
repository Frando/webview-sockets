//! JSON messages on the platform bridge (Android web message listener, wry IPC).

use serde::{Deserialize, Serialize};

use crate::SessionTicket;

/// A request from the injected script: `{"id": 1, "op": "requestSession"}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BridgeRequest {
    /// Request id, echoed in the reply.
    pub id: u64,
    /// Operation; v1 only knows `"requestSession"`.
    pub op: String,
}

impl BridgeRequest {
    /// The only operation in v1.
    pub const OP_REQUEST_SESSION: &'static str = "requestSession";

    /// Parses a bridge message, returning `None` if it is malformed.
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

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
/// `Ok` carries the ticket, `Err` a short reason shown to the page, e.g.
/// `"NotAllowed"`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let req = BridgeRequest::parse(r#"{"id":7,"op":"requestSession"}"#).unwrap();
        assert_eq!(req.id, 7);
        assert_eq!(req.op, BridgeRequest::OP_REQUEST_SESSION);
        assert!(BridgeRequest::parse("{}").is_none());
        let t = SessionTicket {
            url: "ws://a/x".into(),
            token: "ab".into(),
        };
        assert_eq!(
            bridge_reply(7, Ok(&t)),
            r#"{"id":7,"ok":true,"url":"ws://a/x","token":"ab"}"#
        );
        assert_eq!(
            bridge_reply(8, Err("NotAllowed")),
            r#"{"id":8,"ok":false,"error":"NotAllowed"}"#
        );
    }
}
