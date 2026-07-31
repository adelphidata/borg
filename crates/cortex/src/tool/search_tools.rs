//! `search_tools` meta-tool: keyword search over deferred MCP tools.
//!
//! Only advertised on the deferred disclosure tiers (see
//! [`crate::config::McpConfig::disclosure_tier`]). When active, the MCP tool
//! schemas are held out of the per-turn tool list; the model searches here to
//! discover a tool's spec, then calls the real `mcp_*` tool directly the next
//! turn. Dispatch is registry-keyed, so a discovered tool is callable even
//! though it was never advertised.

use serde::Deserialize;

/// Args for the `search_tools` meta-tool. Both fields optional: with no `query`
/// the tool returns a browse index of every deferred tool.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SearchToolsArgs {
    /// Keyword(s) matched (case-insensitively) against each tool's name and
    /// description. Absent or empty → browse index instead of a keyword search.
    #[serde(default)]
    query: Option<String>,
    /// Max full specs to return for a keyword search. Clamped to
    /// `1..=`[`crate::mcp_clients::TOOL_SEARCH_LIMIT_MAX`] by the registry
    /// search; defaults to [`DEFAULT_LIMIT`] when omitted.
    #[serde(default)]
    limit: Option<usize>,
}

/// Default number of full specs returned for a keyword search. Advertised to
/// the model via the `search_tools` definition — kept `pub(crate)` so that
/// definition states this single source rather than restating the number.
pub(crate) const DEFAULT_LIMIT: usize = 5;

/// Execute the search against the live MCP registry. Pure registry read (sync);
/// returns a JSON string per [`crate::mcp_clients::search_registered_tools`].
pub(crate) fn handle(args: &SearchToolsArgs) -> String {
    crate::mcp_clients::search_registered_tools(
        args.query.as_deref(),
        args.limit.unwrap_or(DEFAULT_LIMIT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_parse_with_all_fields_omitted() {
        // The tool must be callable with `{}` — the model discovers the whole
        // surface via the browse index when it doesn't yet know what to search.
        let args: SearchToolsArgs = serde_json::from_value(json!({})).expect("parse empty");
        assert!(args.query.is_none());
        assert!(args.limit.is_none());
    }

    #[test]
    fn handle_returns_structured_json_even_with_no_registry() {
        // In a unit-test process the global registry is unset; the handler must
        // still return well-formed JSON with an explicit note (never a silent
        // empty string), so the model always gets an actionable answer.
        let args: SearchToolsArgs =
            serde_json::from_value(json!({"query": "issue"})).expect("parse");
        let out = handle(&args);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["tools"].is_array(), "must carry a tools array");
        assert!(
            parsed["note"].as_str().is_some_and(|n| !n.is_empty()),
            "must carry a non-empty note explaining the result"
        );
    }
}
