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
        /// Path to a PEM bundle of extra certificate authorities to trust for
        /// this server. Added to the built-in roots, never replacing them, so
        /// this cannot be used to weaken verification for anything else.
        ca_cert_path: Option<String>,
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
        /// Extra certificate authorities to trust (see [`Transport::Http`]).
        ca_cert_path: Option<String>,
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
            return list_matches(&self.include, tool_name);
        }
        !list_matches(&self.exclude, tool_name)
    }

    /// True iff the operator has marked `tool_name` as safe to call in Plan
    /// mode. Glob entries apply here exactly as in [`Self::admits`] — a
    /// `read_only` list that silently no-oped on patterns would be a
    /// Plan-mode trust hole in the other direction (operator *believes* the
    /// family is vetted, nothing actually is).
    pub fn is_read_only(&self, tool_name: &str) -> bool {
        list_matches(&self.read_only, tool_name)
    }

    /// Entries from `list` (any of `include`/`exclude`/`read_only`) that
    /// contain glob metacharacters but matched none of `tool_names`.
    /// Surfaced as a registration-time warning: an unmatched pattern used to
    /// silently match nothing, so a config meant to trim a large server
    /// actually changed nothing.
    pub fn unmatched_patterns<'a>(list: &'a [String], tool_names: &[&str]) -> Vec<&'a str> {
        list.iter()
            .filter(|entry| is_glob_entry(entry))
            .filter(|entry| !tool_names.iter().any(|name| glob_match(entry, name)))
            .map(String::as_str)
            .collect()
    }
}

/// True iff `tool_name` matches any entry in `list`. Entries without glob
/// metacharacters stay strictly literal (`"docs"` never matches
/// `"docs_search"`); entries containing `*`, `?`, or `[` are glob patterns.
/// Exact membership is checked first so literal lists keep their O(n)
/// string-compare cost.
fn list_matches(list: &[String], tool_name: &str) -> bool {
    if list.iter().any(|n| n == tool_name) {
        return true;
    }
    list.iter()
        .any(|entry| is_glob_entry(entry) && glob_match(entry, tool_name))
}

/// True when `entry` should be treated as a glob pattern rather than a
/// literal name.
fn is_glob_entry(entry: &str) -> bool {
    entry.contains(['*', '?', '['])
}

/// Minimal glob matcher: `*` any run (including empty), `?` exactly one
/// char, `[abc]` / `[a-z]` / `[!abc]` char classes. An unterminated `[`
/// matches a literal `[`. Case-sensitive, matching the exact-name semantics
/// of the surrounding filter.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_at(&pat, 0, &txt, 0)
}

fn glob_match_at(pat: &[char], mut p: usize, txt: &[char], mut t: usize) -> bool {
    // Backtracking points for the most recent `*`.
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;

    while t < txt.len() {
        if p < pat.len() {
            match pat[p] {
                '*' => {
                    // Record the star and greedily try to match nothing first.
                    star_p = Some(p);
                    star_t = t;
                    p += 1;
                    continue;
                }
                '?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, next_p)) = match_char_class(pat, p, txt[t]) {
                        if matched {
                            p = next_p;
                            t += 1;
                            continue;
                        }
                    } else if txt[t] == '[' {
                        // Unterminated class: literal `[`.
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                c if c == txt[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: backtrack to the last `*`, consuming one more text char.
        match star_p {
            Some(sp) => {
                p = sp + 1;
                star_t += 1;
                t = star_t;
            }
            None => return false,
        }
    }
    // Text consumed; remaining pattern must be all `*`.
    pat[p..].iter().all(|&c| c == '*')
}

/// Match `c` against the char class opening at `pat[start]` (which is `[`).
/// Returns `(matched, index_past_class)`, or `None` when the class is
/// unterminated.
fn match_char_class(pat: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = pat.get(i) == Some(&'!');
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == ']' && !first {
            return Some((matched != negated, i + 1));
        }
        // Range form `a-z` (a `-` at the edges is literal).
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            if pat[i] <= c && c <= pat[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if pat[i] == c {
                matched = true;
            }
            i += 1;
        }
        first = false;
    }
    None
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

    // ── Glob entries ──

    #[test]
    fn glob_entries_match_families_literals_stay_literal() {
        // A flat-API server declaring hundreds of `*_radar_*` tools can be
        // trimmed with one pattern; the literal "docs" entry must never widen
        // to "docs_search".
        let f = ToolFilter {
            exclude: vec!["*_radar_*".into(), "docs".into()],
            ..Default::default()
        };
        assert!(!f.admits("get_radar_report"), "glob must exclude family");
        assert!(!f.admits("accounts_radar_x"));
        assert!(!f.admits("docs"), "literal still excludes exact name");
        assert!(f.admits("docs_search"), "literal must stay literal");
        assert!(f.admits("radar"), "*_radar_* needs both underscores");
    }

    #[test]
    fn glob_include_and_question_and_class_forms() {
        let f = ToolFilter {
            include: vec!["get_?".into(), "purge_[cd]ache".into()],
            ..Default::default()
        };
        assert!(f.admits("get_a"), "? matches exactly one char");
        assert!(!f.admits("get_ab"));
        assert!(!f.admits("get_"));
        assert!(f.admits("purge_cache"), "char class member matches");
        assert!(f.admits("purge_dache"));
        assert!(!f.admits("purge_ecache"));
    }

    #[test]
    fn glob_negated_class_and_range() {
        let f = ToolFilter {
            include: vec!["v[0-9]".into(), "x[!a]".into()],
            ..Default::default()
        };
        assert!(f.admits("v7"));
        assert!(!f.admits("va"));
        assert!(f.admits("xb"));
        assert!(!f.admits("xa"));
    }

    #[test]
    fn glob_applies_to_read_only_identically() {
        // A read_only pattern that silently no-oped would leave the operator
        // believing a family is Plan-mode vetted when nothing is.
        let f = ToolFilter {
            read_only: vec!["list_*".into(), "get_item".into()],
            ..Default::default()
        };
        assert!(f.is_read_only("list_records"));
        assert!(f.is_read_only("list_"));
        assert!(f.is_read_only("get_item"));
        assert!(!f.is_read_only("get_items"), "literal stays literal");
        assert!(!f.is_read_only("delete_record"));
    }

    #[test]
    fn unmatched_patterns_reports_only_dead_globs() {
        let list = vec![
            "*_radar_*".into(),
            "purge_cache".into(), // literal — never reported even if absent
            "list_*".into(),
        ];
        let declared = ["list_records", "delete_record"];
        let dead = ToolFilter::unmatched_patterns(&list, &declared);
        assert_eq!(dead, vec!["*_radar_*"]);
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
