//! JSON-RPC 2.0 envelopes, as used on Moonraker's websocket (`/websocket`).
//!
//! Moonraker answers requests with `{"jsonrpc": "2.0", "result": ..., "id": ...}` or
//! `{"jsonrpc": "2.0", "error": {"code": ..., "message": ...}, "id": ...}`, and pushes
//! notifications as `{"jsonrpc": "2.0", "method": "notify_...", "params": [...]}`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: &str = "2.0";

/// Standard JSON-RPC error codes that Moonraker uses.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Absent for notifications sent by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl Request {
    pub fn new(method: impl Into<String>, params: Option<Value>, id: Option<Value>) -> Self {
        Self { jsonrpc: VERSION.to_string(), method: method.into(), params, id }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Any message a server sends on the websocket.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Reply to a request: `Ok(result)` or `Err(error)`, keyed by the request id.
    Response { id: Value, outcome: Result<Value, ErrorObject> },
    /// Server-initiated notification.
    Notification { method: String, params: Value },
}

impl Incoming {
    /// Classifies one decoded websocket message. Returns `None` for anything that's neither a
    /// response nor a notification.
    pub fn from_value(msg: Value) -> Option<Self> {
        let Value::Object(mut obj) = msg else { return None };
        if let Some(method) = obj.get("method").and_then(Value::as_str).map(str::to_owned) {
            let params = obj.remove("params").unwrap_or(Value::Null);
            return Some(Self::Notification { method, params });
        }
        let id = obj.remove("id")?;
        if let Some(err) = obj.remove("error") {
            let err = serde_json::from_value(err).ok()?;
            return Some(Self::Response { id, outcome: Err(err) });
        }
        let result = obj.remove("result")?;
        Some(Self::Response { id, outcome: Ok(result) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_messages() {
        let ok = Incoming::from_value(json!({"jsonrpc": "2.0", "result": "ok", "id": 4})).unwrap();
        assert_eq!(ok, Incoming::Response { id: json!(4), outcome: Ok(json!("ok")) });

        let err = Incoming::from_value(json!({"jsonrpc": "2.0", "id": 5,
            "error": {"code": -32601, "message": "Method not found"}}))
        .unwrap();
        let Incoming::Response { outcome: Err(e), .. } = err else { panic!() };
        assert_eq!(e.code, codes::METHOD_NOT_FOUND);

        let note = Incoming::from_value(json!({"jsonrpc": "2.0", "method": "notify_klippy_ready"})).unwrap();
        assert_eq!(note, Incoming::Notification { method: "notify_klippy_ready".into(), params: Value::Null });
    }
}
