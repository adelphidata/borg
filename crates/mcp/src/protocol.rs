//! JSON-RPC 2.0 framing + the minimal MCP method bodies we use.
//!
//! Hand-rolled rather than pulling in a third-party MCP SDK: the wire
//! surface borg cares about is small (`initialize`, `tools/list`,
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
    /// Requires either a JSON-RPC code in the auth range *or* a message
    /// substring unambiguous about credentials. "permission" alone is NOT
    /// classified as auth — servers use it for `tools/call` runtime errors
    /// like "missing required permission scope" which are recoverable
    /// per-call, not credential failures.
    pub fn looks_like_auth(&self) -> bool {
        const AUTH_CODES: &[i32] = &[-32001, -32002, -32003];
        if AUTH_CODES.contains(&self.code) {
            return true;
        }
        message_looks_like_auth(&self.message)
    }
}

/// Substring heuristic: does this free-text error message unambiguously
/// indicate an authentication/authorization failure?
///
/// Extracted from [`RpcError::looks_like_auth`] so the startup-failure
/// classifier can reuse the *exact same* markers when an auth signal is
/// buried inside a transport-wrapped error (e.g. a gateway that returns an
/// auth body under a 5xx, or an auth failure flattened into a "transport
/// closed" string) — the JSON-RPC code path can't see those, but the message
/// text still carries the signal.
///
/// Conservative on purpose: `"permission"` and a bare `"auth"` are NOT
/// matched (servers use them for recoverable per-call runtime errors). A
/// false positive here only mislabels a server as `auth-failed` instead of
/// its transient reason — strictly safer than a credential-spamming retry
/// loop, matching the posture already documented above.
pub fn message_looks_like_auth(message: &str) -> bool {
    if message_has_auth_wording(message) {
        return true;
    }
    // Bare HTTP status codes are meaningful in a *short, server-supplied*
    // JSON-RPC error message, so the direct-peer path admits them. They are
    // deliberately NOT part of [`message_has_auth_wording`]: over a verbose,
    // multi-cause error chain "401"/"403" collides with ports, timeouts, byte
    // counts, and trace ids (see that fn's doc), so the startup classifier
    // must not scan for them.
    let m = message.to_ascii_lowercase();
    m.contains("401") || m.contains("403")
}

/// Word-only auth markers, safe to scan over *verbose, multi-cause* error text
/// (a flattened `{err:#}` anyhow chain full of transport strings, connect
/// `host:port`, and operator-chosen server names).
///
/// Unlike [`message_looks_like_auth`], this omits the bare `"401"`/`"403"`
/// status-code substrings: those are unambiguous in a short JSON-RPC message
/// but false-positive badly over a full error chain — a connect failure to
/// `:8403`, a `4013ms` timeout, a `req-4013ab` trace id, or a server literally
/// named `my401k-bot` would each be misread as an auth failure and send the
/// operator down a spurious "re-credential" path for a transient outage. Word
/// markers (`unauthorized`, `forbidden`, `authentication`, `auth required`,
/// `auth failed`) carry no such collision risk.
pub fn message_has_auth_wording(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("unauthor")
        || m.contains("forbidden")
        // "auth" alone is broad but in combination with these terms is
        // unambiguous about credentials.
        || m.contains("auth required")
        || m.contains("auth failed")
        || m.contains("authentication")
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

/// Capability advertisement. Declares nothing — we only consume
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
    /// Optional behavior hints advertised by the server. Borg only consumes
    /// `readOnlyHint` today (to flag the tool as parallel-safe with other
    /// read-only tools); other hints are accepted but currently ignored.
    /// The hint never widens Plan-mode visibility — that remains gated on
    /// explicit operator opt-in via per-server `tools.read_only`.
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

/// Subset of MCP tool annotations Borg consumes. Untrusted — a malicious
/// server can claim `read_only_hint: true` on a tool that mutates. The
/// hint only widens the parallel-safe set (a non-privilege-granting
/// batching decision); Plan-mode visibility still requires explicit
/// operator opt-in via per-server `tools.read_only`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolAnnotations {
    /// Server's claim that this tool does not mutate its environment.
    /// Maps to the MCP `readOnlyHint` field.
    #[serde(rename = "readOnlyHint", default)]
    pub read_only_hint: Option<bool>,
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
    /// a placeholder ("[unsupported content block: <kind>]").
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
    fn message_looks_like_auth_matches_only_unambiguous_credential_markers() {
        // The extracted predicate backs both the direct-peer `RpcError` path
        // and the nested-in-transport startup rescue, so its marker set is the
        // single source of truth for "this is a credential failure". Lock the
        // admits and the rejects (case-insensitively) so neither path can drift.
        for msg in [
            "Unauthorized",
            "HTTP 401",
            "got a 403 back",
            "FORBIDDEN",
            "authentication failed",
            "auth required to continue",
            "upstream returned 401 Unauthorized", // nested-in-5xx shape
        ] {
            assert!(
                message_looks_like_auth(msg),
                "{msg:?} should be classified as auth"
            );
        }
        for msg in [
            "permission denied for tool foo",
            "missing required permission scope: read",
            "internal server error",
            "peer closed mid-request",
            "rate limited",
            "timeout after 2000ms",
        ] {
            assert!(
                !message_looks_like_auth(msg),
                "{msg:?} should NOT be classified as auth"
            );
        }
    }

    #[test]
    fn message_has_auth_wording_ignores_bare_status_digits_but_admits_words() {
        // The startup classifier scans this over the whole flattened error
        // chain, so it MUST match auth *words* while ignoring bare "401"/"403"
        // digits that collide with ports, timeouts, and trace ids.
        for msg in [
            "Unauthorized",
            "FORBIDDEN",
            "authentication failed",
            "auth required to continue",
            "upstream returned 401 Unauthorized", // word present alongside code
        ] {
            assert!(
                message_has_auth_wording(msg),
                "{msg:?} should be auth wording"
            );
        }
        for msg in [
            "connect failed: tcp connect error: 10.0.0.5:8403: connection refused",
            "read timed out after 4013ms",
            "handshake failed for server 'my401k-bot'",
            "connection reset after 4015 bytes",
            "trace id req-4013ab",
            "HTTP 401", // bare code with no word: rejected by the word-only matcher
        ] {
            assert!(
                !message_has_auth_wording(msg),
                "{msg:?} must NOT be treated as auth wording"
            );
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

    #[test]
    fn unsupported_content_block_renders_placeholder_not_silent_drop() {
        // Spec: surfacing image/audio/resource as a labeled placeholder is
        // important — the LLM should know a block existed and was elided,
        // not get a silently-truncated response. A regression that emitted
        // an empty string would let the model think the call succeeded
        // with no content.
        let r = ToolCallResult {
            content: vec![
                ContentBlock::Text {
                    text: "before".into(),
                },
                ContentBlock::Other,
                ContentBlock::Text {
                    text: "after".into(),
                },
            ],
            is_error: false,
        };
        let out = render_call_result(&r);
        assert!(
            out.contains("[unsupported mcp content block]"),
            "got: {out}"
        );
        // Ordering must be preserved so the model sees the text-around-blob
        // structure rather than a reordered transcript.
        let pre = out.find("before").expect("before");
        let placeholder = out.find("[unsupported").expect("placeholder");
        let post = out.find("after").expect("after");
        assert!(pre < placeholder && placeholder < post);
    }

    #[test]
    fn other_variant_decodes_from_unknown_block_type() {
        // Real-world failure: a future MCP spec block type (e.g. "video")
        // must NOT fail decode of the whole response — it should land in
        // `Other` so we degrade gracefully. Encode the spec.
        let resp = br#"{
            "content":[
                {"type":"image","data":"..."},
                {"type":"text","text":"caption"}
            ],
            "isError":false
        }"#;
        let result: ToolCallResult = serde_json::from_slice(resp).expect("decode");
        assert_eq!(result.content.len(), 2);
        assert!(matches!(result.content[0], ContentBlock::Other));
        match &result.content[1] {
            ContentBlock::Text { text } => assert_eq!(text, "caption"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn empty_content_renders_empty_string_not_an_error() {
        // A tools/call response with `content: []` and `isError: false` is
        // legal MCP — render to an empty string, don't panic / pad with
        // a placeholder. The caller's `is_error` flag is the failure signal.
        let r = ToolCallResult {
            content: vec![],
            is_error: false,
        };
        assert_eq!(render_call_result(&r), "");
    }

    #[test]
    fn is_error_flag_decodes_when_present_and_defaults_false() {
        // `isError` is the authoritative tool-failure signal. Both shapes —
        // present `true`, and absent (must default to `false`) — are common
        // and must round-trip correctly.
        let with_error: ToolCallResult =
            serde_json::from_str(r#"{"content":[],"isError":true}"#).expect("decode");
        assert!(with_error.is_error);

        let without: ToolCallResult = serde_json::from_str(r#"{"content":[]}"#).expect("decode");
        assert!(!without.is_error, "absent isError must default to false");
    }

    #[test]
    fn tools_list_decodes_with_optional_annotations() {
        // Both shapes the wire produces: with and without `annotations`.
        // A regression that made `annotations` required would silently
        // reject every minimal-spec server.
        let resp = br#"{
            "tools":[
                {"name":"a","inputSchema":{"type":"object"}},
                {"name":"b","inputSchema":{"type":"object"},
                 "annotations":{"readOnlyHint":true}}
            ]
        }"#;
        let list: ToolsListResult = serde_json::from_slice(resp).expect("decode");
        assert_eq!(list.tools.len(), 2);
        assert_eq!(list.tools[0].name, "a");
        assert!(list.tools[0].annotations.is_none());

        let ann = list.tools[1]
            .annotations
            .as_ref()
            .expect("second tool has annotations");
        assert_eq!(ann.read_only_hint, Some(true));
    }

    #[test]
    fn client_info_matches_borg_identity() {
        // Servers log clientInfo for triage. The constants must stay
        // borg-branded (no upstream-name leakage) and the version must
        // be the actual crate version, not a static placeholder.
        let info = ClientInfo::standard();
        assert_eq!(info.name, "borg");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn decode_line_surfaces_offending_prefix_in_error() {
        // Spec: when a peer sends garbage, the operator needs to see what
        // we tried to decode. Locking the prefix-in-error contract so a
        // refactor doesn't silently drop the diagnostic payload.
        let err = decode_line(b"not even json").expect_err("must fail");
        match err {
            McpError::Protocol(msg) => {
                assert!(msg.contains("not even json"), "must echo bytes: {msg}");
                assert!(msg.contains("decode failed"), "must explain: {msg}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn decode_line_truncates_diagnostic_to_256_bytes() {
        // Defense: a malicious peer that sends megabytes of garbage must
        // not blow up the audit log. The decoder caps the echoed payload
        // at 256 bytes. Regression: a refactor that drops the cap would
        // silently make logs grow unboundedly under attack.
        let big = vec![b'x'; 4096];
        let err = decode_line(&big).expect_err("must fail");
        let msg = err.to_string();
        // The error string includes more than just the echoed bytes (e.g.
        // the serde error and "decode failed" prefix). Bound the total
        // length conservatively to catch the unbounded case.
        assert!(
            msg.len() < 1024,
            "diagnostic should be bounded, got {} bytes",
            msg.len()
        );
    }
}
