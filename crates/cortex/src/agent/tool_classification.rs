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

/// Args-aware variant of [`is_mutating_tool`] — the gate the agent loop must
/// use. Some tools (today `computer_use`) pass the name-level allowlist while
/// only *some* of their actions are read-only; the per-action logic lives in
/// [`crate::tool::readonly_tool_call`] next to the name allowlist so the two
/// can't drift. Unparseable args or a missing / unrecognised `action` are
/// treated as mutating so malformed calls and future actions default to
/// blocked in Plan mode. MCP tools keep the name-level trust policy above.
pub(crate) fn is_mutating_tool_call(name: &str, raw_args: &str) -> bool {
    if name.starts_with("mcp_") {
        return is_mutating_tool(name);
    }
    !crate::tool::readonly_tool_call(name, raw_args)
}

/// Tools that pass the name-level Plan-mode allowlist but are only read-only
/// for *some* of their actions — the mutation decision needs the call's
/// arguments (see [`crate::tool::readonly_tool_call`]). A flat name list like
/// [`mutating_tool_names`] structurally can't represent "sometimes mutating",
/// so this is the single place that names them: the parent per-call gate
/// dispatches per-action for these, and sub-agent delegation blocks the whole
/// tool (a static blocklist can't express per-action). Both consumers read
/// this slice so the two enforcement paths can't drift.
pub fn conditionally_mutating_tool_names() -> &'static [&'static str] {
    &[crate::tool::names::COMPUTER_USE]
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
        "transcribe_audio",
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
        "web_fetch" | "web_search" | "browser" | "text_to_speech" | "transcribe_audio"
        | "generate_image" | "vision_analyze" => ActionType::WebRequest,
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
            ("transcribe_audio", ActionType::WebRequest),
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
            "transcribe_audio",
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

    /// Plan-mode regression guard for the per-action computer_use gate.
    /// The name-level allowlist admits `computer_use` (capture/list_apps/
    /// wait are read-only), so before this gate existed a Plan-mode agent
    /// could click/type on the host. Read-only actions pass; every mutating
    /// action and every malformed payload is blocked.
    #[test]
    fn computer_use_gate_blocks_mutating_actions_only() {
        let readonly = [
            r#"{"action":"capture"}"#,
            r#"{"action":"list_apps"}"#,
            r#"{"action":"wait","duration_ms":100}"#,
        ];
        for args in readonly {
            assert!(
                !is_mutating_tool_call("computer_use", args),
                "read-only computer_use call must pass Plan mode: {args}"
            );
        }
        let mutating = [
            r#"{"action":"click","x":1,"y":2}"#,
            r#"{"action":"double_click","x":1,"y":2}"#,
            r#"{"action":"right_click","x":1,"y":2}"#,
            r#"{"action":"middle_click","x":1,"y":2}"#,
            r#"{"action":"drag","x":1,"y":2,"to_x":3,"to_y":4}"#,
            r#"{"action":"scroll","delta_y":-3}"#,
            r#"{"action":"type","text":"hi"}"#,
            r#"{"action":"key","key":"Return"}"#,
            r#"{"action":"set_value","element_index":1,"value":"x"}"#,
            r#"{"action":"focus_app","target":"Finder"}"#,
        ];
        for args in mutating {
            assert!(
                is_mutating_tool_call("computer_use", args),
                "mutating computer_use call must be blocked in Plan mode: {args}"
            );
        }
        // Fail-closed rows: unparseable JSON, missing action, unknown action.
        for args in ["not json", "{}", r#"{"action":"launch_missiles"}"#, ""] {
            assert!(
                is_mutating_tool_call("computer_use", args),
                "malformed computer_use call must fail closed: {args:?}"
            );
        }
    }

    /// Non-computer_use tools must keep name-level semantics: the args
    /// string is irrelevant even when it *looks* like a read-only action.
    #[test]
    fn gate_ignores_args_for_name_classified_tools() {
        assert!(is_mutating_tool_call(
            "run_shell",
            r#"{"action":"capture"}"#
        ));
        assert!(!is_mutating_tool_call("read_file", "not json at all"));
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
