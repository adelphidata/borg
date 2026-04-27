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
/// preventing accidental mutation in plan mode.
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    !matches!(
        name,
        "read_file"
            | "list_dir"
            | "list"
            | "list_skills"
            | "list_channels"
            | "list_agents"
            | "read_memory"
            | "memory_search"
            | "web_fetch"
            | "web_search"
    )
}

/// Names of every tool that [`is_mutating_tool`] considers mutating.
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
        "generate_image",
        "manage_cron",
        "manage_roles",
        "manage_tasks",
        "projects",
        "request_user_input",
        "run_shell",
        "schedule",
        "send_to_agent",
        "spawn_agent",
        "text_to_speech",
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
        "web_fetch" | "web_search" | "browser" | "text_to_speech" | "generate_image" => {
            ActionType::WebRequest
        }
        _ => ActionType::ToolCall,
    }
}
