//! Strongly-typed MCP error surface.
//!
//! The variants here are the *only* failure shapes the registry promises to
//! the cortex adapter. The adapter maps each variant to a structured tool
//! result the LLM can reason about (`error.kind = "rate_limited" | "auth" |
//! "schema" | …`) so that failures are never silently swallowed.

use std::fmt;
use thiserror::Error;

/// All error shapes returned by the MCP crate.
#[derive(Debug, Error)]
pub enum McpError {
    /// Initial connect failed (DNS, TCP, TLS, handshake before `initialize`).
    #[error("connect failed: {0}")]
    Connect(String),

    /// `initialize` succeeded but server returned an authentication failure
    /// (401/403, or stdio process exited claiming missing credentials).
    ///
    /// Auth failures never auto-retry — the registry stops the session and
    /// surfaces a "fix credentials" warning to the operator.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Underlying transport closed unexpectedly (subprocess died, socket
    /// reset, EOF on stdio). The session supervisor reconnects with exp
    /// backoff up to a bounded number of attempts.
    #[error("transport closed: {0}")]
    TransportClosed(String),

    /// Tool call exceeded its per-call timeout.
    #[error("timeout after {ms}ms")]
    Timeout {
        /// Milliseconds elapsed when the timeout fired.
        ms: u64,
    },

    /// Per-server rate limit refused the call.
    #[error("rate limited (server={server}, retry_after_ms={retry_after_ms})")]
    RateLimited {
        /// Server name that throttled the call.
        server: String,
        /// Suggested wait before retrying.
        retry_after_ms: u64,
    },

    /// A tool's JSON schema failed the registration gauntlet (size, depth,
    /// `$ref`, property count, missing `type:object` root, …).
    ///
    /// The offending tool is not registered; other tools from the same server
    /// may still be usable.
    #[error("schema rejected for {server}/{tool}: {reason}")]
    SchemaRejected {
        /// Server the tool came from.
        server: String,
        /// Original (pre-sanitization) tool name.
        tool: String,
        /// Human-readable reason for rejection. Surfaces in `borg mcp status`.
        reason: SchemaRejectionReason,
    },

    /// Tool name collision: either with a builtin tool name, or with an
    /// already-registered MCP tool. The later registration loses.
    #[error("tool name collision: {full_name} (server={server})")]
    NameCollision {
        /// Server attempting the registration.
        server: String,
        /// The full namespaced name (`mcp_<server>_<tool>`).
        full_name: String,
    },

    /// HTTP transport refused a URL (plaintext when not allowed, loopback /
    /// RFC1918 when not allowed, non-https, etc.).
    #[error("URL refused by transport policy: {url} ({reason})")]
    UrlRefused {
        /// The refused URL.
        url: String,
        /// Why the policy refused it.
        reason: UrlRefusalReason,
    },

    /// Protocol-level error from the peer (JSON-RPC `error` field, or a
    /// malformed response we can't parse).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// I/O failure not otherwise classified.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Why a tool's schema was rejected at registration time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaRejectionReason {
    /// Serialized schema exceeded the per-tool size cap.
    TooLarge {
        /// Observed serialized size in bytes.
        bytes: usize,
    },
    /// Schema root was not `{"type":"object", …}`.
    NotObjectRoot,
    /// Schema used `$ref`, which we do not resolve (external-ref attack
    /// surface).
    ContainsRef,
    /// Property count exceeded the cap.
    TooManyProperties {
        /// Number of properties observed.
        count: usize,
    },
    /// Per-server `tools.max` cap reached; no more registrations accepted
    /// from this server.
    PerServerCapReached,
    /// Global `mcp.max_tools_total` cap reached.
    GlobalCapReached,
    /// Sanitized tool name was empty (e.g. all-non-alphanumeric source name).
    EmptyName,
}

impl fmt::Display for SchemaRejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes } => write!(f, "schema too large ({bytes} bytes)"),
            Self::NotObjectRoot => write!(f, "schema root is not type:object"),
            Self::ContainsRef => write!(f, "schema contains $ref"),
            Self::TooManyProperties { count } => {
                write!(f, "schema declares {count} properties (cap exceeded)")
            }
            Self::PerServerCapReached => write!(f, "per-server tool cap reached"),
            Self::GlobalCapReached => write!(f, "global tool cap reached"),
            Self::EmptyName => write!(f, "sanitized name was empty"),
        }
    }
}

/// Why the HTTP transport refused a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRefusalReason {
    /// URL used a scheme other than `https`, and `allow_plaintext_http` is
    /// disabled (the default).
    NotHttps,
    /// URL pointed at loopback or RFC1918 space, and `allow_local_network` is
    /// disabled (the default). SSRF guard.
    LocalNetworkBlocked,
    /// URL failed to parse.
    Malformed,
}

impl fmt::Display for UrlRefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotHttps => write!(f, "not https"),
            Self::LocalNetworkBlocked => write!(f, "loopback / RFC1918 blocked"),
            Self::Malformed => write!(f, "malformed url"),
        }
    }
}

/// Convenience result alias.
pub type Result<T, E = McpError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_rejection_display_includes_diagnostic_payload() {
        // The Display output is operator-facing in `borg mcp status` and
        // structured-tool-result JSON. A regression that dropped the
        // numeric byte-count or property-count would surface as a less
        // actionable "schema too large" with no number. Lock the payload.
        assert_eq!(
            SchemaRejectionReason::TooLarge { bytes: 12_345 }.to_string(),
            "schema too large (12345 bytes)"
        );
        assert_eq!(
            SchemaRejectionReason::TooManyProperties { count: 99 }.to_string(),
            "schema declares 99 properties (cap exceeded)"
        );
        assert_eq!(
            SchemaRejectionReason::NotObjectRoot.to_string(),
            "schema root is not type:object"
        );
        assert_eq!(
            SchemaRejectionReason::ContainsRef.to_string(),
            "schema contains $ref"
        );
        assert_eq!(
            SchemaRejectionReason::PerServerCapReached.to_string(),
            "per-server tool cap reached"
        );
        assert_eq!(
            SchemaRejectionReason::GlobalCapReached.to_string(),
            "global tool cap reached"
        );
        assert_eq!(
            SchemaRejectionReason::EmptyName.to_string(),
            "sanitized name was empty"
        );
    }

    #[test]
    fn url_refusal_display_distinguishes_reasons() {
        // Distinct phrasing per variant lets operators disambiguate
        // "switch to https" vs "flip allow_local_network" without reading
        // the source. Catch any reword that collapses the variants.
        assert_eq!(UrlRefusalReason::NotHttps.to_string(), "not https");
        assert_eq!(
            UrlRefusalReason::LocalNetworkBlocked.to_string(),
            "loopback / RFC1918 blocked"
        );
        assert_eq!(UrlRefusalReason::Malformed.to_string(), "malformed url");
    }

    #[test]
    fn mcp_error_display_surfaces_inner_context() {
        // Spec: every variant carries enough context that an operator
        // can act without log-grepping. The schema/URL/rate-limit/timeout
        // variants embed their structured payloads in Display; locking
        // the exact wording protects the audit log format.
        let timeout = McpError::Timeout { ms: 2_500 };
        assert_eq!(timeout.to_string(), "timeout after 2500ms");

        let rate = McpError::RateLimited {
            server: "github".into(),
            retry_after_ms: 4_000,
        };
        assert_eq!(
            rate.to_string(),
            "rate limited (server=github, retry_after_ms=4000)"
        );

        let schema = McpError::SchemaRejected {
            server: "notion".into(),
            tool: "ghost".into(),
            reason: SchemaRejectionReason::ContainsRef,
        };
        let s = schema.to_string();
        assert!(s.contains("notion/ghost"), "must name server/tool: {s}");
        assert!(s.contains("$ref"), "must surface inner reason: {s}");

        let url = McpError::UrlRefused {
            url: "http://10.0.0.1".into(),
            reason: UrlRefusalReason::LocalNetworkBlocked,
        };
        let s = url.to_string();
        assert!(s.contains("http://10.0.0.1"), "must echo URL: {s}");
        assert!(s.contains("loopback / RFC1918 blocked"), "must explain reason: {s}");

        let collision = McpError::NameCollision {
            server: "github".into(),
            full_name: "mcp_github_search".into(),
        };
        let s = collision.to_string();
        assert!(s.contains("mcp_github_search"));
        assert!(s.contains("server=github"));
    }

    #[test]
    fn io_and_json_errors_convert_via_from() {
        // The #[from] derivations are how transport code wraps low-level
        // failures without losing the original cause. A breaking refactor
        // (e.g. removing #[from]) would surface as a compile error here,
        // not silently change the type at runtime.
        let io: McpError = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "rst").into();
        match io {
            McpError::Io(inner) => assert_eq!(inner.kind(), std::io::ErrorKind::ConnectionReset),
            other => panic!("expected Io variant, got {other:?}"),
        }

        let parse_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let json: McpError = parse_err.into();
        assert!(matches!(json, McpError::Json(_)));
        assert!(json.to_string().starts_with("json error:"));
    }
}
