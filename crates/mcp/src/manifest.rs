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
    },
    /// Server-Sent Events flavour of MCP HTTP.
    Sse {
        /// SSE endpoint URL.
        url: String,
        /// Headers (typically the same as Http).
        headers: HashMap<String, String>,
        /// Optional structured auth.
        auth: HttpAuth,
    },
}

/// HTTP auth flavour. Phase 1 supports None (use `headers` directly) and
/// Bearer (set `Authorization: Bearer <token>` automatically).
///
/// OAuth 2.1 PKCE is deferred to Phase 2.
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
}
