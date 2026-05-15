//! JSON-RPC 2.0 wire types for the codex `app-server` subprocess.
//!
//! Transport is newline-delimited JSON-RPC 2.0 over stdio. Three frame kinds:
//!
//! 1. **Response** — `{"jsonrpc":"2.0","id":<n>,"result":...}` or
//!    `{"jsonrpc":"2.0","id":<n>,"error":...}`. Reply to a client request.
//! 2. **Notification** — `{"jsonrpc":"2.0","method":...,"params":...}` (no id).
//!    Streamed item/* events and `turn/completed` arrive this way.
//! 3. **Server-initiated request** — `{"jsonrpc":"2.0","id":<n>,"method":...}`.
//!    Approval prompts arrive this way; the client MUST `respond` with the same id.
//!
//! Codex tags id as either a number or a string; we follow JSON-RPC and accept both.
//! Outbound frames carry `"jsonrpc": "2.0"` per spec.
//!
//! This module is pure data shapes — no IO. Lives next to `client.rs`, which owns
//! the subprocess and reader loop.

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request/notification/response id.
///
/// JSON-RPC permits id to be a string, a number, or null. Codex uses numeric ids
/// for its own requests but echoes back whatever the client sent on responses, so
/// we model the union.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id (codex's choice for server-initiated requests).
    Num(i64),
    /// String id (some MCP-flavored clients use these).
    Str(String),
}

impl RequestId {
    /// Construct a numeric id from a `u64`. Used by the client's monotonic counter.
    pub fn from_u64(n: u64) -> Self {
        Self::Num(n as i64)
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    /// Numeric error code. Standard JSON-RPC codes plus codex-specific extensions.
    pub code: i64,
    /// Human-readable message; safe to log.
    pub message: String,
    /// Optional structured payload (codex sometimes attaches diagnostics here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A frame received from the codex `app-server` stdout.
///
/// Classification is done by field presence (matching JSON-RPC dispatch rules):
///
/// | id present | method present | result/error present | kind            |
/// |------------|----------------|----------------------|-----------------|
/// | yes        | no             | yes                  | `Response`      |
/// | yes        | yes            | no                   | `ServerRequest` |
/// | no         | yes            | n/a                  | `Notification`  |
///
/// Anything else is parsed as `Unknown` and surfaced to diagnostics instead of dropped.
#[derive(Debug, Clone)]
pub enum InboundFrame {
    /// Reply to a request the client sent.
    Response {
        /// Matches the `id` the client put on the request.
        id: RequestId,
        /// `Ok(result)` on success; `Err(err)` on JSON-RPC error.
        result: Result<serde_json::Value, RpcError>,
    },
    /// Streaming event from the server (no id; no reply expected).
    Notification {
        /// Dotted method name, e.g. `item/agentMessage`, `turn/completed`.
        method: String,
        /// Method-specific payload; shape depends on `method`.
        params: serde_json::Value,
    },
    /// Server-initiated request — client must respond using the same id.
    ServerRequest {
        /// The id the server assigned; echoed in the reply.
        id: RequestId,
        /// Method the server is invoking on the client.
        method: String,
        /// Method-specific params.
        params: serde_json::Value,
    },
}

/// Parse a single newline-delimited JSON-RPC frame from the codex stdout stream.
///
/// Returns `None` only when the line is empty after trimming. Garbage that
/// deserializes but doesn't match any frame shape returns `Some(Notification)`
/// with an empty method, surfaced to the caller for diagnostics (we don't drop
/// frames silently — that violates the no-silent-errors invariant).
pub fn parse_frame(line: &str) -> Option<Result<InboundFrame, serde_json::Error>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return Some(Err(e)),
    };
    Some(Ok(classify(value)))
}

fn classify(value: serde_json::Value) -> InboundFrame {
    let obj = match value {
        serde_json::Value::Object(map) => map,
        other => {
            return InboundFrame::Notification {
                method: String::new(),
                params: other,
            };
        }
    };
    let id = obj.get("id").and_then(parse_id);
    let method = obj.get("method").and_then(|v| v.as_str()).map(String::from);
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");

    match (id, method, has_result || has_error) {
        // Valid Response: id + result/error, no method.
        (Some(id), None, true) => InboundFrame::Response {
            id,
            result: decode_response_result(&obj),
        },
        // Malformed Response: JSON-RPC forbids method+result on the same
        // frame. We surface it as a Response (with a synthetic error) rather
        // than burying it in the notification stream — that way the waiting
        // request fails fast instead of timing out.
        (Some(id), Some(method), true) => InboundFrame::Response {
            id,
            result: Err(RpcError {
                code: -1,
                message: format!(
                    "malformed JSON-RPC frame: id + method ({method:?}) + result/error all present"
                ),
                data: None,
            }),
        },
        (Some(id), Some(method), false) => InboundFrame::ServerRequest {
            id,
            method,
            params: obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        (None, Some(method), _) => InboundFrame::Notification {
            method,
            params: obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        _ => InboundFrame::Notification {
            method: String::new(),
            params: serde_json::Value::Object(obj),
        },
    }
}

fn decode_response_result(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, RpcError> {
    if let Some(err) = obj.get("error") {
        match serde_json::from_value::<RpcError>(err.clone()) {
            Ok(e) => Err(e),
            Err(e) => Err(RpcError {
                code: -1,
                message: format!("malformed error payload: {e}"),
                data: None,
            }),
        }
    } else {
        Ok(obj
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

fn parse_id(v: &serde_json::Value) -> Option<RequestId> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().map(RequestId::Num),
        serde_json::Value::String(s) => Some(RequestId::Str(s.clone())),
        _ => None,
    }
}

/// `serde_json::to_string` on a `serde_json::Value` is infallible — every field
/// we encode is already a JSON value, so the only way this could fail is a
/// `serde_json` bug. Panic rather than ship `{}` as the wire frame, which
/// would silently turn into a caller-side timeout with no diagnostic trail.
#[allow(clippy::expect_used)]
fn encode_value(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is always serializable")
}

/// Encode an outbound JSON-RPC request to its on-wire JSON object.
///
/// Caller is responsible for appending the newline framing.
pub fn encode_request(id: &RequestId, method: &str, params: &serde_json::Value) -> String {
    encode_value(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_to_value(id),
        "method": method,
        "params": params,
    }))
}

/// Encode an outbound notification (no id, no response expected).
pub fn encode_notification(method: &str, params: &serde_json::Value) -> String {
    encode_value(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
}

/// Encode a reply to a server-initiated request.
pub fn encode_response(id: &RequestId, result: &serde_json::Value) -> String {
    encode_value(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_to_value(id),
        "result": result,
    }))
}

/// Encode an error reply to a server-initiated request.
pub fn encode_error_response(id: &RequestId, code: i64, message: &str) -> String {
    encode_value(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_to_value(id),
        "error": { "code": code, "message": message },
    }))
}

fn id_to_value(id: &RequestId) -> serde_json::Value {
    match id {
        RequestId::Num(n) => serde_json::Value::Number((*n).into()),
        RequestId::Str(s) => serde_json::Value::String(s.clone()),
    }
}

// ── Method names (string constants for type-safety in callers) ──

/// `initialize` — handshake request, must precede any other method.
pub const METHOD_INITIALIZE: &str = "initialize";
/// `initialized` — notification after `initialize` response is consumed.
pub const METHOD_INITIALIZED: &str = "initialized";
/// `thread/start` — open a new conversation thread.
pub const METHOD_THREAD_START: &str = "thread/start";
/// `turn/start` — begin a turn on the current thread with a user prompt.
pub const METHOD_TURN_START: &str = "turn/start";
/// `turn/interrupt` — abort the in-flight turn (best-effort).
pub const METHOD_TURN_INTERRUPT: &str = "turn/interrupt";
/// `turn/completed` — notification: the in-flight turn ended.
pub const METHOD_TURN_COMPLETED: &str = "turn/completed";
/// `turn/failed` — notification: the in-flight turn errored.
pub const METHOD_TURN_FAILED: &str = "turn/failed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_with_result() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let frame = parse_frame(line).unwrap().unwrap();
        match frame {
            InboundFrame::Response {
                id: RequestId::Num(1),
                result: Ok(v),
            } => {
                assert_eq!(v["ok"], serde_json::Value::Bool(true));
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_with_error() {
        let line =
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"method not found"}}"#;
        let frame = parse_frame(line).unwrap().unwrap();
        match frame {
            InboundFrame::Response {
                id: RequestId::Num(7),
                result: Err(e),
            } => {
                assert_eq!(e.code, -32601);
                assert_eq!(e.message, "method not found");
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn parse_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"item/agentMessage","params":{"text":"hi"}}"#;
        let frame = parse_frame(line).unwrap().unwrap();
        match frame {
            InboundFrame::Notification { method, params } => {
                assert_eq!(method, "item/agentMessage");
                assert_eq!(params["text"], "hi");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_request() {
        let line = r#"{"jsonrpc":"2.0","id":42,"method":"exec/approve","params":{"cmd":"ls"}}"#;
        let frame = parse_frame(line).unwrap().unwrap();
        match frame {
            InboundFrame::ServerRequest { id, method, params } => {
                assert_eq!(id, RequestId::Num(42));
                assert_eq!(method, "exec/approve");
                assert_eq!(params["cmd"], "ls");
            }
            other => panic!("expected ServerRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_string_id_response() {
        let line = r#"{"jsonrpc":"2.0","id":"req-abc","result":null}"#;
        let frame = parse_frame(line).unwrap().unwrap();
        match frame {
            InboundFrame::Response {
                id: RequestId::Str(s),
                result: Ok(serde_json::Value::Null),
            } => assert_eq!(s, "req-abc"),
            other => panic!("expected string-id response, got {other:?}"),
        }
    }

    #[test]
    fn empty_line_returns_none() {
        assert!(parse_frame("").is_none());
        assert!(parse_frame("   \n").is_none());
    }

    #[test]
    fn malformed_json_surfaces_error() {
        let result = parse_frame("not json").unwrap();
        assert!(result.is_err(), "garbage should not be silently dropped");
    }

    #[test]
    fn encode_request_round_trip() {
        let wire = encode_request(
            &RequestId::Num(5),
            "thread/start",
            &serde_json::json!({"meta": "x"}),
        );
        let parsed: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 5);
        assert_eq!(parsed["method"], "thread/start");
        assert_eq!(parsed["params"]["meta"], "x");
    }

    #[test]
    fn encode_notification_has_no_id() {
        let wire = encode_notification(METHOD_INITIALIZED, &serde_json::json!({}));
        let parsed: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert!(parsed.get("id").is_none(), "notifications must omit id");
        assert_eq!(parsed["method"], METHOD_INITIALIZED);
    }

    #[test]
    fn encode_response_round_trip() {
        let wire = encode_response(
            &RequestId::Num(99),
            &serde_json::json!({"decision": "allow"}),
        );
        let parsed: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed["id"], 99);
        assert_eq!(parsed["result"]["decision"], "allow");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn encode_error_response_round_trip() {
        let wire = encode_error_response(&RequestId::Str("x".into()), -32000, "denied");
        let parsed: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed["id"], "x");
        assert_eq!(parsed["error"]["code"], -32000);
        assert_eq!(parsed["error"]["message"], "denied");
    }
}
