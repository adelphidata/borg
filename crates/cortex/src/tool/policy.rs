//! Composable tool policy engine.
//!
//! Resolves which tools are visible to the LLM based on layered policies:
//! 1. Profile policy (from `config.tools.profile`)
//! 2. Global allow/deny (from `config.tools.allow` / `config.tools.deny`)
//! 3. Subagent restrictions (from `config.tools.subagents.deny`)

use std::collections::HashSet;

use crate::config::ToolPolicyConfig;
use crate::tool::catalog::{tool_group, ToolGroup, ToolProfile};
use crate::types::ToolDefinition;

/// Expand a policy entry like `"group:fs"` into individual tool names,
/// or return the entry itself if it's a plain tool name.
fn expand_entry(entry: &str) -> Vec<String> {
    if let Some(group_name) = entry.strip_prefix("group:") {
        if let Some(group) = ToolGroup::from_str_opt(group_name) {
            return group.tool_names().iter().map(ToString::to_string).collect();
        }
    }
    vec![entry.to_string()]
}

/// Expand a list of policy entries (tool names and `group:xxx` references).
fn expand_entries(entries: &[String]) -> HashSet<String> {
    entries.iter().flat_map(|e| expand_entry(e)).collect()
}

/// A [`ToolPolicyConfig`] with its `group:xxx` entries expanded once, so the
/// same decision can be asked per advertised tool and per model-issued call
/// without re-expanding the lists each time.
///
/// This is the single source of truth for "may this session use tool `X`":
/// `build_tool_definitions` uses it to decide what the model is shown, and
/// the pre-dispatch gates in `tool_execution` use it to refuse a call the
/// model issues anyway (a forked child whose inherited history shows the
/// parent calling `run_shell`, an operator deny list the model ignores).
pub struct ResolvedPolicy {
    allowed_groups: HashSet<ToolGroup>,
    explicit_allow: Option<HashSet<String>>,
    explicit_deny: HashSet<String>,
    subagent_deny: HashSet<String>,
}

impl ResolvedPolicy {
    /// Expand `policy` into name sets. Unknown profiles fall back to `full`.
    pub fn new(policy: &ToolPolicyConfig) -> Self {
        let profile = ToolProfile::from_str_opt(&policy.profile).unwrap_or_default();
        Self {
            allowed_groups: profile.groups(),
            explicit_allow: (!policy.allow.is_empty()).then(|| expand_entries(&policy.allow)),
            explicit_deny: expand_entries(&policy.deny),
            subagent_deny: expand_entries(&policy.subagent_deny),
        }
    }

    /// Whether `name` is usable under this policy. `is_subagent` adds the
    /// `subagent_deny` layer. User/MCP/plugin tools (no catalog group) pass
    /// the profile and allow layers; only an explicit deny removes them.
    pub fn permits(&self, name: &str, is_subagent: bool) -> bool {
        let group = tool_group(name);

        // 1. Profile filtering: if tool has a known group, check membership.
        if let Some(group) = group {
            if !self.allowed_groups.contains(&group) {
                return false;
            }
        }

        // 2. Explicit allow: a catalog tool must be listed; user tools pass.
        if let Some(allow_set) = &self.explicit_allow {
            if group.is_some() && !allow_set.contains(name) {
                return false;
            }
        }

        // 3. Deny always wins.
        if self.explicit_deny.contains(name) {
            return false;
        }

        // 4. Sub-agents lose a further set of tools.
        if is_subagent && self.subagent_deny.contains(name) {
            return false;
        }

        true
    }
}

/// One-shot form of [`ResolvedPolicy::permits`] for a single tool name.
pub fn is_permitted(name: &str, policy: &ToolPolicyConfig, is_subagent: bool) -> bool {
    ResolvedPolicy::new(policy).permits(name, is_subagent)
}

/// The tool result handed back to the model when it names a tool this
/// session may not use. Addressable (`Error:` prefix, like the validation
/// layer's synthetic results) so the model can choose another tool.
pub fn unavailable_tool_message(name: &str) -> String {
    format!(
        "Error: tool `{name}` is not available in this session. Use one of the tools you were given."
    )
}

/// Filter tool definitions based on profile and allow/deny policy.
pub fn filter_tools(tools: Vec<ToolDefinition>, policy: &ToolPolicyConfig) -> Vec<ToolDefinition> {
    let resolved = ResolvedPolicy::new(policy);
    tools
        .into_iter()
        .filter(|td| resolved.permits(&td.function.name, false))
        .collect()
}

/// Filter tools for a subagent, applying additional restrictions.
pub fn filter_subagent_tools(
    tools: Vec<ToolDefinition>,
    policy: &ToolPolicyConfig,
) -> Vec<ToolDefinition> {
    let resolved = ResolvedPolicy::new(policy);
    tools
        .into_iter()
        .filter(|td| resolved.permits(&td.function.name, true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDefinition;
    use serde_json::json;

    fn make_tool(name: &str) -> ToolDefinition {
        ToolDefinition::new(name, "test", json!({"type": "object", "properties": {}}))
    }

    fn default_policy() -> ToolPolicyConfig {
        ToolPolicyConfig::default()
    }

    #[test]
    fn full_profile_passes_everything() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("apply_patch"),
            make_tool("browser"),
            make_tool("gmail"),
        ];
        let filtered = filter_tools(tools, &default_policy());
        assert_eq!(filtered.len(), 4);
    }

    #[test]
    fn minimal_profile_filters_most_tools() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("read_memory"),
            make_tool("list"),
            make_tool("apply_patch"),
            make_tool("run_shell"),
            make_tool("browser"),
            make_tool("gmail"),
        ];
        let mut policy = default_policy();
        policy.profile = "minimal".to_string();
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"list"));
        assert!(!names.contains(&"apply_patch"));
        assert!(!names.contains(&"browser"));
        assert!(!names.contains(&"gmail"));
    }

    #[test]
    fn coding_profile_includes_fs_and_runtime() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("apply_patch"),
            make_tool("run_shell"),
            make_tool("gmail"),
            make_tool("browser"),
        ];
        let mut policy = default_policy();
        policy.profile = "coding".to_string();
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"run_shell"));
        assert!(!names.contains(&"gmail"));
        assert!(!names.contains(&"browser"));
    }

    #[test]
    fn deny_overrides_profile() {
        let tools = vec![make_tool("write_memory"), make_tool("read_memory")];
        let mut policy = default_policy();
        policy.deny = vec!["write_memory".to_string()];
        let filtered = filter_tools(tools, &policy);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].function.name, "read_memory");
    }

    #[test]
    fn deny_with_group_expansion() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("read_memory"),
            make_tool("apply_patch"),
        ];
        let mut policy = default_policy();
        policy.deny = vec!["group:memory".to_string()];
        let filtered = filter_tools(tools, &policy);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].function.name, "apply_patch");
    }

    #[test]
    fn explicit_allow_restricts_to_listed_tools() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("apply_patch"),
            make_tool("run_shell"),
        ];
        let mut policy = default_policy();
        policy.allow = vec!["write_memory".to_string(), "apply_patch".to_string()];
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"apply_patch"));
        assert!(!names.contains(&"run_shell"));
    }

    #[test]
    fn user_tools_pass_profile_filter() {
        let tools = vec![make_tool("write_memory"), make_tool("my_custom_tool")];
        let mut policy = default_policy();
        policy.profile = "minimal".to_string();
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"my_custom_tool"));
    }

    #[test]
    fn subagent_deny_applied() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("schedule"),
            make_tool("browser"),
        ];
        let mut policy = default_policy();
        policy.subagent_deny = vec!["schedule".to_string(), "browser".to_string()];
        let filtered = filter_subagent_tools(tools, &policy);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].function.name, "write_memory");
    }

    #[test]
    fn expand_entry_group_reference() {
        let expanded = expand_entry("group:memory");
        assert!(expanded.contains(&"write_memory".to_string()));
        assert!(expanded.contains(&"read_memory".to_string()));
    }

    #[test]
    fn expand_entry_plain_name() {
        let expanded = expand_entry("my_tool");
        assert_eq!(expanded, vec!["my_tool".to_string()]);
    }

    #[test]
    fn expand_entry_unknown_group() {
        let expanded = expand_entry("group:nonexistent");
        assert_eq!(expanded, vec!["group:nonexistent".to_string()]);
    }

    #[test]
    fn messaging_profile_allows_integration_but_not_fs() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("list"),
            make_tool("apply_patch"),
            make_tool("run_shell"),
            make_tool("gmail"),
            make_tool("schedule"),
        ];
        let mut policy = default_policy();
        policy.profile = "messaging".to_string();
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"list"));
        assert!(names.contains(&"gmail"));
        assert!(names.contains(&"schedule"));
        assert!(!names.contains(&"apply_patch"));
        assert!(!names.contains(&"run_shell"));
    }

    #[test]
    fn deny_with_allow_deny_wins() {
        let tools = vec![make_tool("write_memory"), make_tool("read_memory")];
        let mut policy = default_policy();
        policy.allow = vec!["write_memory".to_string(), "read_memory".to_string()];
        policy.deny = vec!["write_memory".to_string()];
        let filtered = filter_tools(tools, &policy);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].function.name, "read_memory");
    }

    #[test]
    fn allow_with_group_expansion() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("read_memory"),
            make_tool("apply_patch"),
            make_tool("run_shell"),
        ];
        let mut policy = default_policy();
        policy.allow = vec!["group:memory".to_string()];
        let filtered = filter_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"read_memory"));
        assert!(!names.contains(&"apply_patch"));
        assert!(!names.contains(&"run_shell"));
    }

    #[test]
    fn subagent_deny_with_group_expansion() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("read_memory"),
            make_tool("apply_patch"),
        ];
        let mut policy = default_policy();
        policy.subagent_deny = vec!["group:fs".to_string()];
        let filtered = filter_subagent_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"read_memory"));
        assert!(!names.contains(&"apply_patch"));
    }

    #[test]
    fn empty_policy_passes_everything() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("apply_patch"),
            make_tool("custom_tool"),
        ];
        let policy = ToolPolicyConfig {
            profile: "full".to_string(),
            allow: vec![],
            deny: vec![],
            subagent_deny: vec![],
        };
        let filtered = filter_tools(tools, &policy);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn invalid_profile_falls_back_to_full() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("browser"),
            make_tool("gmail"),
        ];
        let mut policy = default_policy();
        policy.profile = "nonexistent_profile".to_string();
        let filtered = filter_tools(tools, &policy);
        // Full profile includes everything
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn subagent_filter_applies_normal_policy_first() {
        let tools = vec![
            make_tool("write_memory"),
            make_tool("browser"),
            make_tool("schedule"),
        ];
        let mut policy = default_policy();
        policy.profile = "minimal".to_string();
        policy.subagent_deny = vec!["schedule".to_string()];
        let filtered = filter_subagent_tools(tools, &policy);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        // browser and schedule should both be gone (browser by profile, schedule by subagent deny)
        assert!(names.contains(&"write_memory"));
        assert!(!names.contains(&"browser"));
        assert!(!names.contains(&"schedule"));
    }

    #[test]
    fn default_policy_has_subagent_deny_list() {
        let policy = default_policy();
        assert!(policy.subagent_deny.contains(&"schedule".to_string()));
        assert!(policy.subagent_deny.contains(&"browser".to_string()));
    }

    #[test]
    fn expand_entries_deduplicates() {
        let entries = vec![
            "write_memory".to_string(),
            "group:memory".to_string(), // includes write_memory again
        ];
        let expanded = expand_entries(&entries);
        // HashSet deduplicates
        assert!(expanded.contains("write_memory"));
        assert!(expanded.contains("read_memory"));
    }

    /// The advertised-list filter and the call-time predicate must never
    /// disagree: a tool hidden from the model must also be refused when the
    /// model names it anyway, and a shown tool must run. Expected values are
    /// the policy spec (profile groups, allow narrows catalog tools only,
    /// deny wins, subagent_deny applies to children), not the implementation.
    #[test]
    fn is_permitted_agrees_with_filter_tools_on_every_case() {
        struct Case {
            profile: &'static str,
            allow: &'static [&'static str],
            deny: &'static [&'static str],
            subagent_deny: &'static [&'static str],
            name: &'static str,
            is_subagent: bool,
            expected: bool,
        }
        let cases = [
            Case {
                profile: "full",
                allow: &[],
                deny: &[],
                subagent_deny: &[],
                name: "run_shell",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &["run_shell"],
                subagent_deny: &[],
                name: "run_shell",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &["group:fs"],
                subagent_deny: &[],
                name: "apply_patch",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "minimal",
                allow: &[],
                deny: &[],
                subagent_deny: &[],
                name: "apply_patch",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "minimal",
                allow: &[],
                deny: &[],
                subagent_deny: &[],
                name: "write_memory",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "minimal",
                allow: &[],
                deny: &[],
                subagent_deny: &[],
                name: "mcp_custom_server_tool",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "full",
                allow: &["write_memory"],
                deny: &[],
                subagent_deny: &[],
                name: "run_shell",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &["write_memory"],
                deny: &[],
                subagent_deny: &[],
                name: "write_memory",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "full",
                allow: &["write_memory"],
                deny: &[],
                subagent_deny: &[],
                name: "mcp_custom_server_tool",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "full",
                allow: &["write_memory"],
                deny: &["write_memory"],
                subagent_deny: &[],
                name: "write_memory",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &["mcp_custom_server_tool"],
                subagent_deny: &[],
                name: "mcp_custom_server_tool",
                is_subagent: false,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &[],
                subagent_deny: &["schedule"],
                name: "schedule",
                is_subagent: false,
                expected: true,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &[],
                subagent_deny: &["schedule"],
                name: "schedule",
                is_subagent: true,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &[],
                subagent_deny: &["group:fs"],
                name: "apply_patch",
                is_subagent: true,
                expected: false,
            },
            Case {
                profile: "full",
                allow: &[],
                deny: &[],
                subagent_deny: &["group:fs"],
                name: "write_memory",
                is_subagent: true,
                expected: true,
            },
            Case {
                profile: "no_such_profile",
                allow: &[],
                deny: &[],
                subagent_deny: &[],
                name: "browser",
                is_subagent: false,
                expected: true,
            },
        ];
        let to_vec = |xs: &[&str]| xs.iter().map(ToString::to_string).collect::<Vec<_>>();
        for (i, c) in cases.iter().enumerate() {
            let policy = ToolPolicyConfig {
                profile: c.profile.to_string(),
                allow: to_vec(c.allow),
                deny: to_vec(c.deny),
                subagent_deny: to_vec(c.subagent_deny),
            };
            let predicate = is_permitted(c.name, &policy, c.is_subagent);
            let tools = vec![make_tool(c.name)];
            let shown = if c.is_subagent {
                filter_subagent_tools(tools, &policy)
            } else {
                filter_tools(tools, &policy)
            };
            assert_eq!(
                predicate, c.expected,
                "case {i}: is_permitted({}, profile={}, allow={:?}, deny={:?}, subagent_deny={:?}, sub={})",
                c.name, c.profile, c.allow, c.deny, c.subagent_deny, c.is_subagent
            );
            assert_eq!(
                shown.len() == 1,
                c.expected,
                "case {i}: advertised list disagrees with the spec for {}",
                c.name
            );
        }
    }

    #[test]
    fn unavailable_tool_message_is_an_addressable_error_naming_the_tool() {
        let msg = unavailable_tool_message("run_shell");
        assert!(msg.starts_with("Error:"), "{msg}");
        assert!(msg.contains("`run_shell`"), "{msg}");
    }
}
