//! Composable tool policy engine.
//!
//! Resolves which tools are visible to the LLM based on layered policies:
//! 1. Profile policy (from `config.tools.profile`)
//! 2. Global allow/deny (from `config.tools.allow` / `config.tools.deny`)
//! 3. Subagent restrictions (from `config.tools.subagents.deny`)

use std::collections::HashSet;

use crate::config::ToolPolicyConfig;
use crate::tool_catalog::{tool_group, ToolGroup, ToolProfile};
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

/// Filter tool definitions based on profile and allow/deny policy.
pub fn filter_tools(tools: Vec<ToolDefinition>, policy: &ToolPolicyConfig) -> Vec<ToolDefinition> {
    let profile = ToolProfile::from_str_opt(&policy.profile).unwrap_or_default();
    let allowed_groups = profile.groups();

    let explicit_allow: Option<HashSet<String>> = if policy.allow.is_empty() {
        None
    } else {
        Some(expand_entries(&policy.allow))
    };

    let explicit_deny: HashSet<String> = expand_entries(&policy.deny);

    tools
        .into_iter()
        .filter(|td| {
            let name = td.function.name.as_str();

            // 1. Profile filtering: if tool has a known group, check membership
            if let Some(group) = tool_group(name) {
                if !allowed_groups.contains(&group) {
                    return false;
                }
            }
            // User tools (no group) always pass profile filtering

            // 2. Explicit allow: if set, tool must be in the list
            if let Some(ref allow_set) = explicit_allow {
                if !allow_set.contains(name) {
                    // User tools not in allow list are still permitted
                    if tool_group(name).is_some() {
                        return false;
                    }
                }
            }

            // 3. Deny always wins
            if explicit_deny.contains(name) {
                return false;
            }

            true
        })
        .collect()
}

/// Filter tools for a subagent, applying additional restrictions.
pub fn filter_subagent_tools(
    tools: Vec<ToolDefinition>,
    policy: &ToolPolicyConfig,
) -> Vec<ToolDefinition> {
    // First apply normal policy
    let mut filtered = filter_tools(tools, policy);

    // Then apply subagent deny list
    let subagent_deny: HashSet<String> = expand_entries(&policy.subagent_deny);
    if !subagent_deny.is_empty() {
        filtered.retain(|td| !subagent_deny.contains(td.function.name.as_str()));
    }

    filtered
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
}
