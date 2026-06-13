//! Transport-layer config types.
//!
//! The cortex-side `McpServerConfig` owns the operator-facing shape (settings
//! table, secret-ref resolution, plan-mode classification). The cortex
//! adapter resolves secrets and constructs a [`ServerManifest`] from it,
//! which is what this crate actually consumes.
//!
//! Keeping the two types distinct means the MCP crate stays free of any
//! coupling to the `cortex::security::secrets_resolve` module — secrets are
//! already plain strings by the time they reach us, which makes the
//! transport layer trivially testable in isolation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Resolved per-server manifest passed into [`crate::McpRegistry`].
///
/// All `${SECRET:…}` placeholders have already been resolved by the caller;
/// `env` and `headers` here contain plaintext values that the transport will
/// forward.
#[derive(Debug, Clone)]
pub struct ServerManifest {
    /// Operator-chosen server name. Used to namespace tools (`mcp_<name>_*`)
    /// and to tag audit events.
    pub name: String,
    /// Transport-specific connection details.
    pub transport: Transport,
    /// How long the initial handshake may take before failing.
    pub connect_timeout: Duration,
    /// Per-call timeout for `tools/call`.
    pub call_timeout: Duration,
    /// Per-minute steady-state call rate.
    pub rate_limit_per_minute: u32,
    /// Burst capacity before the rate limiter starts refusing.
    pub rate_limit_burst: u32,
    /// Tool filter (include / exclude / read-only / per-server max).
    pub tools: ToolFilter,
}

/// Transport-specific connection details.
#[derive(Debug, Clone)]
pub enum Transport {
    /// Local subprocess speaking newline-delimited JSON-RPC over stdio.
    Stdio {
        /// Executable to spawn.
        command: String,
        /// CLI args.
        args: Vec<String>,
        /// Additional environment variables to pass through (on top of the
        /// hardcoded allowlist). Resolved-secret values, not placeholders.
        env: HashMap<String, String>,
    },
    /// Streamable HTTP MCP endpoint.
    Http {
        /// Endpoint URL (must be `https://` unless `allow_plaintext_http`).
        url: String,
        /// Headers to send on every request (Authorization, etc.).
        headers: HashMap<String, String>,
        /// Optional structured auth that the transport may handle specially.
        auth: HttpAuth,
        /// Optional client certificate presented during the TLS handshake
        /// (mutual TLS). `None` for the common server-auth-only case.
        client_identity: Option<ClientIdentity>,
    },
    /// Server-Sent Events flavour of MCP HTTP.
    Sse {
        /// SSE endpoint URL.
        url: String,
        /// Headers (typically the same as Http).
        headers: HashMap<String, String>,
        /// Optional structured auth.
        auth: HttpAuth,
        /// Optional client certificate for mutual TLS (see [`Transport::Http`]).
        client_identity: Option<ClientIdentity>,
    },
}

/// A client certificate + private key presented during the TLS handshake so a
/// server requiring mutual TLS accepts the connection. Paths are resolved
/// (tilde-expanded, absolute) by the time they reach the transport; the PEM is
/// loaded at connect time so a missing/invalid file fails fast.
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    /// Path to the client certificate PEM. May also contain the private key
    /// (combined PEM), in which case `key_path` is `None`.
    pub cert_path: String,
    /// Path to the private key PEM when it lives in a separate file.
    pub key_path: Option<String>,
}

/// HTTP auth flavour. Supports None (use `headers` directly) and
/// Bearer (set `Authorization: Bearer <token>` automatically).
#[derive(Debug, Clone, Default)]
pub enum HttpAuth {
    /// Use `headers` as-is.
    #[default]
    None,
    /// Set `Authorization: Bearer <token>`.
    Bearer {
        /// Resolved bearer token.
        token: String,
    },
}

impl HttpAuth {
    /// Apply this auth scheme to an outbound header map, setting
    /// `Authorization: Bearer <token>` when a bearer token is configured.
    /// [`HttpAuth::None`] leaves the headers untouched.
    pub fn apply_to(&self, headers: &mut HashMap<String, String>) {
        if let HttpAuth::Bearer { token } = self {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
    }
}

/// Per-server tool filter.
///
/// `include` wins over `exclude`. `read_only` flips plan-mode classification —
/// only tools the operator has explicitly vetted are callable in Plan mode.
/// `max` caps how many tools a single server can register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFilter {
    /// If non-empty, only tools whose original (pre-sanitization) MCP name
    /// matches one of these strings are registered.
    #[serde(default)]
    pub include: Vec<String>,
    /// Tools to exclude. Ignored if `include` is non-empty.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Original MCP tool names the operator has marked safe to call in Plan
    /// mode. Anything not in this list is treated as mutating (blocked in
    /// Plan mode).
    #[serde(default)]
    pub read_only: Vec<String>,
    /// Per-server cap. Defaults to [`ToolFilter::DEFAULT_MAX`].
    #[serde(default = "ToolFilter::default_max")]
    pub max: usize,
}

impl ToolFilter {
    /// Per-server tool cap default. Combined with the global
    /// `mcp.max_tools_total` cap (default 128) to bound prompt-context
    /// growth.
    pub const DEFAULT_MAX: usize = 32;

    fn default_max() -> usize {
        Self::DEFAULT_MAX
    }

    /// True iff `tool_name` (the *original* MCP name, before sanitization)
    /// passes the include/exclude filter.
    pub fn admits(&self, tool_name: &str) -> bool {
        if !self.include.is_empty() {
            return self.include.iter().any(|n| n == tool_name);
        }
        !self.exclude.iter().any(|n| n == tool_name)
    }

    /// True iff the operator has marked `tool_name` as safe to call in Plan
    /// mode.
    pub fn is_read_only(&self, tool_name: &str) -> bool {
        self.read_only.iter().any(|n| n == tool_name)
    }
}

impl Default for ToolFilter {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            read_only: Vec::new(),
            max: Self::DEFAULT_MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_wins_over_exclude() {
        let f = ToolFilter {
            include: vec!["search".into()],
            exclude: vec!["search".into(), "delete".into()],
            ..Default::default()
        };
        // Whitelist semantics: include is exhaustive when set.
        assert!(f.admits("search"));
        assert!(!f.admits("delete"));
        assert!(!f.admits("anything_else"));
    }

    #[test]
    fn exclude_only_blocks_listed() {
        let f = ToolFilter {
            exclude: vec!["delete".into()],
            ..Default::default()
        };
        assert!(f.admits("search"));
        assert!(!f.admits("delete"));
    }

    #[test]
    fn read_only_is_explicit_opt_in() {
        let f = ToolFilter {
            read_only: vec!["search".into()],
            ..Default::default()
        };
        // Encodes the spec invariant: external servers cannot self-declare
        // read-only — only the operator config can.
        assert!(f.is_read_only("search"));
        assert!(!f.is_read_only("delete"));
        assert!(!f.is_read_only("unlisted"));
    }

    #[test]
    fn default_filter_is_open_admit_anything_nothing_read_only() {
        // Spec: the default filter is "let everything through, nothing
        // privileged for Plan mode". A regression that flipped the default
        // to a deny-by-default would silently disable freshly-added MCP
        // servers — fail loudly here instead.
        let f = ToolFilter::default();
        assert!(f.admits("anything"));
        assert!(f.admits(""));
        assert!(!f.is_read_only("anything"));
        assert_eq!(f.max, ToolFilter::DEFAULT_MAX);
        assert_eq!(ToolFilter::DEFAULT_MAX, 32);
    }

    #[test]
    fn empty_exclude_admits_everything() {
        let f = ToolFilter::default();
        for name in ["a", "search", "delete", "anything"] {
            assert!(f.admits(name), "default must admit {name}");
        }
    }

    #[test]
    fn include_is_exact_match_not_substring() {
        // Real-world failure: an `include` of "search" accidentally
        // admitting "search_pro" would widen the operator-curated set.
        // Reject substring matching.
        let f = ToolFilter {
            include: vec!["search".into()],
            ..Default::default()
        };
        assert!(f.admits("search"));
        assert!(!f.admits("search_pro"));
        assert!(!f.admits("pre_search"));
    }

    #[test]
    fn http_auth_default_is_none() {
        // Spec: the *absence* of structured auth must not silently inject
        // an `Authorization: Bearer <empty>` header. Default is None;
        // operators opt into Bearer explicitly.
        let auth = HttpAuth::default();
        assert!(
            matches!(auth, HttpAuth::None),
            "default HttpAuth must be None, got {auth:?}"
        );
    }

    #[test]
    fn http_auth_apply_to_sets_bearer_only_when_present() {
        // None must not touch the header map — preserves operator-supplied
        // headers and never injects an empty Authorization.
        let mut headers = HashMap::new();
        HttpAuth::None.apply_to(&mut headers);
        assert!(headers.is_empty());

        // Bearer injects exactly `Authorization: Bearer <token>`.
        let mut headers = HashMap::new();
        HttpAuth::Bearer {
            token: "sekret".into(),
        }
        .apply_to(&mut headers);
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer sekret")
        );
    }

    #[test]
    fn tool_filter_deserializes_with_defaults() {
        // An empty JSON object must deserialize to the open default —
        // operator config is allowed to omit any subset of fields without
        // surprising-default surprises.
        let f: ToolFilter = serde_json::from_str("{}").expect("default deserialize");
        assert!(f.include.is_empty());
        assert!(f.exclude.is_empty());
        assert!(f.read_only.is_empty());
        assert_eq!(f.max, ToolFilter::DEFAULT_MAX);
    }

    #[test]
    fn tool_filter_partial_deserialize_honors_specified_fields() {
        // Mixed config: only `include` and `max` specified. Everything
        // else must still apply defaults — protects callers that grow
        // the config gradually.
        let f: ToolFilter =
            serde_json::from_str(r#"{"include":["a","b"],"max":4}"#).expect("partial");
        assert_eq!(f.include, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(f.max, 4);
        assert!(f.exclude.is_empty());
        assert!(f.read_only.is_empty());
        assert!(f.admits("a"));
        assert!(!f.admits("c"));
    }
}
