//! Normalized ingress message types.

use serde::{Deserialize, Serialize};

/// Where this message entered the daemon.
///
/// `String` payload is the surface-specific name: channel name (e.g.
/// `"telegram"`), HTTP client identifier (token label), webhook
/// definition name, or MCP client identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum IngressSource {
    /// Native channel adapter (telegram, slack, discord, …).
    Channel(String),
    /// OpenAI-compatible HTTP client identified by token label.
    Http(String),
    /// Generic signed webhook by definition name.
    Webhook(String),
    /// MCP client identifier.
    Mcp(String),
}

impl IngressSource {
    /// Stable string label for audit/logging — `"channel:telegram"`,
    /// `"http:my-token"`, etc. Distinct from `Display` so we can change
    /// human-facing rendering without breaking audit replay.
    pub fn audit_label(&self) -> String {
        match self {
            IngressSource::Channel(name) => format!("channel:{name}"),
            IngressSource::Http(client) => format!("http:{client}"),
            IngressSource::Webhook(name) => format!("webhook:{name}"),
            IngressSource::Mcp(client) => format!("mcp:{client}"),
        }
    }
}

/// One attachment on an inbound message — image, audio, document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// MIME type (e.g. `"image/png"`).
    pub mime_type: String,
    /// Base64-encoded binary content.
    pub data: String,
    /// Original filename if the platform provided one.
    #[serde(default)]
    pub filename: Option<String>,
}

/// Body of a normalized ingress message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    /// Plain-text content. May be empty when the message is purely
    /// attachments (e.g. an image-only Telegram message).
    pub text: String,
    /// Inline media attachments, in arrival order.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl MessageBody {
    /// Construct a text-only body.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            attachments: Vec::new(),
        }
    }

    /// True if the body has neither text nor attachments — the gate
    /// rejects empty bodies so surfaces don't spawn no-op agent turns.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
}

/// Identifier of the authenticated principal that produced this
/// ingress. v1 has a single hard-coded `local` principal; the mTLS
/// path replaces it with the cert-derived identity from the gRPC
/// connection context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrincipalId(String);

impl PrincipalId {
    /// Literal identity used by surfaces that haven't been wired to a
    /// cert-derived principal yet (e.g. the in-process audit harness).
    pub const LOCAL_LITERAL: &'static str = "local";

    /// The single v1 local principal.
    pub fn local() -> Self {
        Self(Self::LOCAL_LITERAL.to_string())
    }

    /// Wrap an explicit identity (the mTLS interceptor calls this with a
    /// value derived from the client cert SAN).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One normalized ingress message produced by an adapter.
///
/// Every adapter (channel, OpenAI-compat, webhook, MCP) emits this
/// shape so downstream session resolution and routing is shared code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressMessage {
    /// Origin surface + surface-specific name.
    pub source: IngressSource,
    /// Authenticated principal (v1: always `local`).
    pub principal: PrincipalId,
    /// Platform-native sender id (telegram chat id, slack user id,
    /// webhook-extracted field, OpenAI token id, …).
    pub sender_id: String,
    /// Platform-native thread/conversation id (slack `thread_ts`, teams
    /// `reply_to_id`, discord `channel_id`, OpenAI conversation extn).
    /// Optional — direct messages with no thread set this to `None`.
    pub thread_id: Option<String>,
    /// Message text + attachments.
    pub body: MessageBody,
    /// Raw provider payload for audit. Adapters store the unparsed JSON
    /// they received so the audit chain can reconstruct exactly what
    /// arrived. Pass `Value::Null` when there's no useful raw form.
    pub meta: serde_json::Value,
    /// Receive timestamp (Unix seconds). Set by the adapter at the
    /// earliest point — before queueing — so latency measurements
    /// reflect arrival, not dispatch.
    pub received_at: i64,
}

/// Build the per-session key used by the agent loop and `channel_sessions`
/// table. Format: `{sender_id}:{thread_id}` when threaded,
/// `{sender_id}:` when not.
///
/// Stable across surfaces — OpenAI-compat, webhook, channel, MCP all
/// route through this function, so the same logical "conversation"
/// always maps to the same session row regardless of surface.
pub fn session_key(sender_id: &str, thread_id: Option<&str>) -> String {
    match thread_id {
        Some(tid) if !tid.is_empty() => format!("{sender_id}:{tid}"),
        _ => format!("{sender_id}:"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_is_stable_across_thread_presence() {
        // The session-key format is the contract every ingress surface
        // depends on for routing into the right session row. If this
        // changes, channel_sessions lookups silently miss and users
        // see a fresh agent state mid-conversation. Pinning the exact
        // format here is the regression guard.
        assert_eq!(session_key("u1", Some("t1")), "u1:t1");
        assert_eq!(session_key("u1", None), "u1:");
        assert_eq!(
            session_key("u1", Some("")),
            "u1:",
            "empty thread_id must collapse to the no-thread form so a \
             surface that emits empty-string instead of None doesn't \
             create a shadow session"
        );
    }

    #[test]
    fn session_key_differs_for_distinct_threads() {
        assert_ne!(session_key("u1", Some("t1")), session_key("u1", Some("t2")));
        assert_ne!(session_key("u1", Some("t1")), session_key("u2", Some("t1")));
    }

    #[test]
    fn audit_label_namespaces_by_source_kind() {
        // Audit-chain queries filter by source kind — a channel named
        // "http" must not collide with the http surface. The `kind:`
        // prefix is what makes that lookup unambiguous.
        assert_eq!(
            IngressSource::Channel("http".into()).audit_label(),
            "channel:http"
        );
        assert_eq!(
            IngressSource::Http("http".into()).audit_label(),
            "http:http"
        );
        assert_eq!(
            IngressSource::Webhook("alerts".into()).audit_label(),
            "webhook:alerts"
        );
        assert_eq!(
            IngressSource::Mcp("desktop".into()).audit_label(),
            "mcp:desktop"
        );
    }

    #[test]
    fn ingress_source_serde_roundtrip() {
        // Audit log persists IngressSource as JSON — round-trip must
        // be lossless or replays drop the surface name.
        for src in [
            IngressSource::Channel("telegram".into()),
            IngressSource::Http("dev-token".into()),
            IngressSource::Webhook("alerts".into()),
            IngressSource::Mcp("claude-desktop".into()),
        ] {
            let json = serde_json::to_string(&src).expect("serialize");
            let back: IngressSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(src, back);
        }
    }

    #[test]
    fn message_body_is_empty_only_when_no_text_and_no_attachments() {
        assert!(MessageBody::text("").is_empty());
        assert!(!MessageBody::text("hi").is_empty());
        let with_att = MessageBody {
            text: String::new(),
            attachments: vec![Attachment {
                mime_type: "image/png".into(),
                data: "AAAA".into(),
                filename: None,
            }],
        };
        assert!(!with_att.is_empty());
    }
}
