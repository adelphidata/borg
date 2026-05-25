//! Classification of tool names by side-effect category.
//!
//! Used by the agent loop to enforce Plan-mode read-only policy and by the
//! rate guard to bucket tool invocations. The two functions here (plus
//! [`mutating_tool_names`]) are the single source of truth for that policy.

use crate::rate_guard::ActionType;

/// Returns `true` if a tool is NOT in the read-only allowlist.
/// Used to block mutating tools in Plan mode.
///
/// Uses an allowlist of known-safe tools so that new tools default to blocked,
/// preventing accidental mutation in plan mode. Delegates to the typed
/// [`crate::tool::allowed_in_plan_mode_name`] so the read-only set has a
/// single source of truth on the [`crate::tool::Tool`] enum.
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    // MCP tools live outside the typed Tool enum. Plan mode is a hard
    // trust boundary, so we ignore untrusted server-advertised
    // `annotations.readOnlyHint` here and only honor explicit operator
    // opt-in via per-server `tools.read_only`. (The hint-widened flag,
    // `mcp_clients::is_read_only_mcp_tool`, is used for parallel batching
    // — a softer decision that doesn't grant any new privileges.)
    // Default-on-miss is `false` (treated as mutating, blocked in Plan
    // mode).
    if name.starts_with("mcp_") {
        return !crate::mcp_clients::is_operator_read_only_mcp_tool(name);
    }
    !crate::tool::allowed_in_plan_mode_name(name)
}

/// Names of every tool that `is_mutating_tool` considers mutating.
///
/// Kept in sync with that function so callers (e.g. sub-agent delegation in
/// Plan mode) can union this list into a child's tool blocklist without
/// having to iterate every possible tool name themselves.
pub fn mutating_tool_names() -> &'static [&'static str] {
    &[
        "apply_patch",
        "apply_skill_patch",
        "browser",
        "close_agent",
        "create_channel",
        "create_goal",
        "generate_image",
        "manage_cron",
        "manage_roles",
        "manage_tasks",
        "projects",
        "run_shell",
        "schedule",
        "send_to_agent",
        "spawn_agent",
        "text_to_speech",
        "update_goal",
        "wait_for_agent",
        "write_memory",
    ]
}

/// Map a tool name to the rate-limit bucket it should count against.
pub(crate) fn classify_action(tool_name: &str) -> ActionType {
    match tool_name {
        "run_shell" => ActionType::ShellCommand,
        "apply_patch" | "apply_skill_patch" | "create_channel" => ActionType::FileWrite,
        "write_memory" => ActionType::MemoryWrite,
        "memory_search" | "read_memory" => ActionType::ToolCall,
        "web_fetch" | "web_search" | "browser" | "text_to_speech" | "generate_image"
        | "vision_analyze" => ActionType::WebRequest,
        _ => ActionType::ToolCall,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_action_bucket_table() {
        // Real-world failure: a refactor moves `run_shell` out of the
        // `ShellCommand` arm — the rate guard for shell commands silently
        // stops firing while the LLM keeps shell-spamming. Lock the
        // bucket-per-tool mapping. Each arm of the match is one row here.
        let cases: &[(&str, ActionType)] = &[
            ("run_shell", ActionType::ShellCommand),
            ("apply_patch", ActionType::FileWrite),
            ("apply_skill_patch", ActionType::FileWrite),
            ("create_channel", ActionType::FileWrite),
            ("write_memory", ActionType::MemoryWrite),
            ("memory_search", ActionType::ToolCall),
            ("read_memory", ActionType::ToolCall),
            ("web_fetch", ActionType::WebRequest),
            ("web_search", ActionType::WebRequest),
            ("browser", ActionType::WebRequest),
            ("text_to_speech", ActionType::WebRequest),
            ("generate_image", ActionType::WebRequest),
            ("vision_analyze", ActionType::WebRequest),
        ];
        for (tool, expected) in cases {
            assert_eq!(
                classify_action(tool),
                *expected,
                "tool {tool} should map to {expected:?}"
            );
        }
    }

    #[test]
    fn classify_action_unknown_tool_falls_back_to_tool_call() {
        // Spec: unrecognized tools must still count against *some* bucket
        // so they're rate-limited. Defaulting to ToolCall (the generic
        // bucket) is the safe default — switching to e.g. "no bucket"
        // would let a malicious / hallucinated tool burn through the loop.
        for unknown in [
            "totally_made_up_tool",
            "mcp_github_search",
            "",
            "schedule",
            "spawn_agent",
        ] {
            assert_eq!(
                classify_action(unknown),
                ActionType::ToolCall,
                "unknown tool {unknown:?} must default to ToolCall"
            );
        }
    }

    #[test]
    fn mutating_tool_names_includes_known_mutators() {
        // Spec: the union returned here must contain every tool that
        // mutates external state. A drop here would silently let a
        // sub-agent inherit a forbidden tool in Plan mode.
        let names = mutating_tool_names();
        for required in [
            "apply_patch",
            "run_shell",
            "write_memory",
            "create_channel",
            "spawn_agent",
            "schedule",
            // Cost-incurring outbound-API tools: TTS hits paid voice
            // providers, generate_image hits DALL-E/FAL. A Plan-mode
            // sub-agent that could call these would burn the operator's
            // budget for "read-only" exploration — keep them blocked.
            "text_to_speech",
            "generate_image",
        ] {
            assert!(
                names.contains(&required),
                "mutating_tool_names() must include {required}"
            );
        }
    }

    #[test]
    fn mutating_tool_names_excludes_read_only_tools() {
        // Inverse spec: read-only tools must NEVER appear in the
        // mutating list — otherwise sub-agents in Plan mode lose access
        // to memory_search etc. and the agent can't reason.
        let names = mutating_tool_names();
        for read_only in [
            "memory_search",
            "read_memory",
            "read_file",
            "list_dir",
            "web_fetch",
            "web_search",
            "list",
            "get_goal",
            "request_user_input",
        ] {
            assert!(
                !names.contains(&read_only),
                "mutating_tool_names() must NOT include read-only tool {read_only}"
            );
        }
    }

    #[test]
    fn mutating_tool_names_has_no_duplicates() {
        // Cheap invariant: duplicates indicate a merge mistake and would
        // hide a typo in one entry behind a working duplicate.
        let names = mutating_tool_names();
        let mut sorted: Vec<&&str> = names.iter().collect();
        sorted.sort();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len, "duplicate in mutating_tool_names()");
    }
}
