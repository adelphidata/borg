//! JSON-RPC 2.0 framing + the minimal MCP method bodies we use.
//!
//! Hand-rolled rather than pulling in a third-party MCP SDK: the wire
//! surface borg cares about in Phase 1 is small (`initialize`, `tools/list`,
//! `tools/call`, the `notifications/tools/list_changed` notification), and
//! borg's security posture prefers auditable bytes over transitive deps.
//!
//! The MCP spec (2025-06-18) wraps tool results in a list of `content`
//! blocks; we model only the text and json subset since that's the
//! agent-loop-relevant surface.

use crate::errors::{McpError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC protocol version we speak.
pub const JSONRPC_VERSION: &str = "2.0";

/// MCP protocol version we declare during `initialize`. The peer may
/// negotiate down; we accept any version they propose in response.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Outbound JSON-RPC request envelope.
#[derive(Debug, Clone, Serialize)]
pub struct Request<'a> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Monotonic per-session integer.
    pub id: u64,
    /// MCP method name.
    pub method: &'a str,
    /// Method parameters; `None` serializes as no `params` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl<'a> Request<'a> {
    /// Build a request with the standard envelope fields filled in.
    pub fn new(id: u64, method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params,
        }
    }
}

/// Inbound JSON-RPC response or notification.
///
/// MCP servers send both: responses to our requests (with an `id` matching
/// one we sent) and notifications (no `id`, e.g. `notifications/tools/list_changed`).
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    /// Always `"2.0"`. We don't enforce strictly — some peers omit it on
    /// notifications.
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// `Some` for responses; `None` for notifications.
    pub id: Option<u64>,
    /// `Some` for notifications and incoming requests we may handle.
    #[serde(default)]
    pub method: Option<String>,
    /// `Some` for successful responses.
    #[serde(default)]
    pub result: Option<Value>,
    /// `Some` for error responses.
    #[serde(default)]
    pub error: Option<RpcError>,
    /// `Some` for incoming notifications / requests.
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    /// JSON-RPC error code. MCP reserves -32xxx for protocol-level errors.
    pub code: i32,
    /// Short human-readable message.
    pub message: String,
    /// Optional structured detail.
    #[serde(default)]
    pub data: Option<Value>,
}

impl RpcError {
    /// Heuristic: is this error from the peer plausibly an auth failure?
    ///
    /// MCP doesn't formalize auth-failure codes; we err toward classifying
    /// as auth so the supervisor stops retrying (a wrong-classification
    /// false-positive is safer than a credential-spamming retry loop).
    ///
    /// Tightened (vs an earlier draft) to require either a JSON-RPC code in
    /// the auth range *or* a message substring that's unambiguous about
    /// credentials. The word "permission" alone is dropped: servers use it
    /// for `tools/call` runtime errors like "missing required permission
    /// scope" which are recoverable per-call, not credential failures.
    pub fn looks_like_auth(&self) -> bool {
        const AUTH_CODES: &[i32] = &[-32001, -32002, -32003];
        if AUTH_CODES.contains(&self.code) {
            return true;
        }
        let m = self.message.to_ascii_lowercase();
        m.contains("unauthor")
            || m.contains("forbidden")
            || m.contains("401")
            || m.contains("403")
            // "auth" alone is broad but in combination with these terms is
            // unambiguous about credentials.
            || m.contains("auth required")
            || m.contains("auth failed")
            || m.contains("authentication")
    }
}

/// `initialize` request params we send.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams<'a> {
    /// MCP protocol version we propose.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'a str,
    /// Capabilities we (the client) support.
    pub capabilities: ClientCapabilities,
    /// Client identification — surfaces in server logs.
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Capability advertisement. Phase 1 declares nothing — we only consume
/// `tools`, not resources / prompts / sampling.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientCapabilities {}

/// Sent in `initialize` so servers can log who connected.
#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    /// Always `"borg"`.
    pub name: &'static str,
    /// Crate version.
    pub version: &'static str,
}

impl ClientInfo {
    /// Standard borg client info used by every session.
    pub fn standard() -> Self {
        Self {
            name: "borg",
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// `initialize` response we expect back. Most fields are advisory; we only
/// look at `serverInfo` for the audit log.
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    /// Negotiated protocol version.
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: Option<String>,
    /// Capabilities the server advertised.
    #[serde(default)]
    pub capabilities: Value,
    /// Server identification, surfaced in audit events.
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<ServerInfo>,
}

/// Optional server-identification block.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version string.
    pub version: String,
}

/// `tools/list` response: a flat list of tool schemas.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolsListResult {
    /// Discovered tools.
    pub tools: Vec<McpToolSchema>,
}

/// One tool entry as advertised by an MCP server.
///
/// Borg validates this through the schema gauntlet before registering — any
/// rejection is reported via [`crate::errors::SchemaRejectionReason`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolSchema {
    /// Original (pre-sanitization) tool name as the server advertises it.
    pub name: String,
    /// Human-readable description; surfaces in the LLM tool list.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's input. Must be `{"type":"object", …}`.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// `tools/call` response, normalized to the agent-loop-relevant fields.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallResult {
    /// Ordered content blocks. We surface `text` and `json` to the LLM.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Server-reported error flag. When true, the call "succeeded" at the
    /// RPC layer but the tool itself reported a failure — borg surfaces this
    /// as a structured error result to the LLM (never silently swallows).
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One content block in a `tools/call` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    /// Text content. The common case.
    Text {
        /// Text body.
        text: String,
    },
    /// Structured JSON content.
    Json {
        /// JSON body.
        json: Value,
    },
    /// Anything else: image, audio, resource link — surfaced to the agent as
    /// a placeholder ("[unsupported content block: <kind>]"). Phase 2 may
    /// surface images/audio properly.
    #[serde(other)]
    Other,
}

impl ContentBlock {
    /// Render this block to a single string the LLM can consume. Distinct
    /// from `Display` because we may want to redact / wrap upstream.
    pub fn render_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Json { json } => serde_json::to_string(json).unwrap_or_default(),
            Self::Other => "[unsupported mcp content block]".to_string(),
        }
    }
}

/// Render an entire [`ToolCallResult`] to text. Used by the cortex adapter
/// before applying the trust-boundary wrapper + redaction.
pub fn render_call_result(result: &ToolCallResult) -> String {
    let mut out = String::new();
    for (i, block) in result.content.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&block.render_text());
    }
    out
}

/// Method names — a tiny module so typos don't compile.
pub mod methods {
    /// `initialize` request.
    pub const INITIALIZE: &str = "initialize";
    /// `notifications/initialized` (client → server, sent after `initialize`).
    pub const INITIALIZED_NOTIFICATION: &str = "notifications/initialized";
    /// `tools/list` request.
    pub const TOOLS_LIST: &str = "tools/list";
    /// `tools/call` request.
    pub const TOOLS_CALL: &str = "tools/call";
    /// `notifications/tools/list_changed` (server → client).
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
}

/// Decode a single inbound JSON line into a [`Message`]. Returns a typed
/// protocol error rather than a raw `serde_json::Error` so callers can log
/// the offending bytes once.
pub fn decode_line(bytes: &[u8]) -> Result<Message> {
    serde_json::from_slice::<Message>(bytes).map_err(|e| {
        McpError::Protocol(format!(
            "decode failed ({e}): {}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_with_jsonrpc_envelope() {
        let req = Request::new(7, methods::TOOLS_LIST, None);
        let s = serde_json::to_string(&req).unwrap();
        // The envelope shape is wire-load-bearing — peers reject messages
        // without it. Lock the exact shape.
        assert!(s.contains("\"jsonrpc\":\"2.0\""), "got: {s}");
        assert!(s.contains("\"id\":7"), "got: {s}");
        assert!(s.contains("\"method\":\"tools/list\""), "got: {s}");
        // No `params` key when None.
        assert!(!s.contains("params"), "got: {s}");
    }

    #[test]
    fn decode_response_and_notification() {
        let resp = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}";
        let m = decode_line(resp).unwrap();
        assert_eq!(m.id, Some(1));
        assert!(m.result.is_some());
        assert!(m.method.is_none());

        let notif = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}";
        let m = decode_line(notif).unwrap();
        assert_eq!(m.id, None);
        assert_eq!(m.method.as_deref(), Some(methods::TOOLS_LIST_CHANGED));
    }

    #[test]
    fn rpc_error_auth_classification_admits_unambiguous_auth_shapes() {
        // Encodes the spec: any unambiguous auth-shaped error stops the
        // supervisor from hot-looping. The auth-code range alone is enough
        // even with a benign-sounding message.
        for (code, msg) in [
            (-32001, "Unauthorized"),
            (-32002, "auth required"),
            (-32603, "Forbidden (403)"),
            (-32603, "401 not allowed"),
            (-32603, "authentication failure"),
            (-32001, ""), // code alone, no message
        ] {
            let e = RpcError {
                code,
                message: msg.into(),
                data: None,
            };
            assert!(
                e.looks_like_auth(),
                "code={code} msg={msg:?} should be auth"
            );
        }
    }

    #[test]
    fn rpc_error_auth_classification_rejects_runtime_permission_errors() {
        // Spec-regression guard: the *word* "permission" alone is not an
        // auth-credential failure. Servers use it for per-call runtime
        // errors like "missing required permission scope: read", which the
        // supervisor must *not* treat as a credential lockout (otherwise
        // recoverable failures kill the session).
        for msg in [
            "permission denied for tool foo",
            "missing required permission scope: read",
            "internal server error",
            "rate limited",
        ] {
            let e = RpcError {
                code: -32603,
                message: msg.into(),
                data: None,
            };
            assert!(!e.looks_like_auth(), "{msg} should NOT be auth");
        }
    }

    #[test]
    fn content_blocks_render_text_and_json() {
        let r = ToolCallResult {
            content: vec![
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::Json {
                    json: json!({"k": 1}),
                },
            ],
            is_error: false,
        };
        assert_eq!(render_call_result(&r), "hello\n{\"k\":1}");
    }
}
