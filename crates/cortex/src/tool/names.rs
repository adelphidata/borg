//! Centralized tool name constants.
//!
//! Single source of truth for tool name strings used across tool_definitions,
//! tool_catalog, agent dispatch, and tool handlers.

// ── Core tools ──
pub const WRITE_MEMORY: &str = "write_memory";
pub const READ_MEMORY: &str = "read_memory";
pub const MEMORY_SEARCH: &str = "memory_search";
pub const LIST: &str = "list";
pub const APPLY_PATCH: &str = "apply_patch";
pub const RUN_SHELL: &str = "run_shell";
pub const READ_FILE: &str = "read_file";
pub const LIST_DIR: &str = "list_dir";

// ── Web tools ──
pub const WEB_FETCH: &str = "web_fetch";
pub const WEB_SEARCH: &str = "web_search";

// ── Scheduling ──
pub const SCHEDULE: &str = "schedule";
pub const MANAGE_TASKS: &str = "manage_tasks";
pub const MANAGE_CRON: &str = "manage_cron";

// ── Media tools ──
pub const BROWSER: &str = "browser";
pub const GENERATE_IMAGE: &str = "generate_image";
pub const TEXT_TO_SPEECH: &str = "text_to_speech";
pub const TRANSCRIBE_AUDIO: &str = "transcribe_audio";
pub const VISION_ANALYZE: &str = "vision_analyze";
pub const VIDEO_ANALYZE: &str = "video_analyze";
pub const COMPUTER_USE: &str = "computer_use";

// ── User interaction ──
pub const REQUEST_USER_INPUT: &str = "request_user_input";

// ── Goals ──
pub const GET_GOAL: &str = "get_goal";
pub const CREATE_GOAL: &str = "create_goal";
pub const UPDATE_GOAL: &str = "update_goal";

// ── Filesystem aliases ──
pub const APPLY_SKILL_PATCH: &str = "apply_skill_patch";
pub const CREATE_CHANNEL: &str = "create_channel";

// ── Discovery aliases ──
pub const LIST_SKILLS: &str = "list_skills";
pub const LIST_CHANNELS: &str = "list_channels";
pub const LIST_AGENTS: &str = "list_agents";
/// Meta-tool that searches deferred external (MCP) tools by keyword and
/// returns their specs on demand. Only advertised when `mcp.tool_search` is on.
pub const SEARCH_TOOLS: &str = "search_tools";

// ── Multi-agent tools ──
pub const SPAWN_AGENT: &str = "spawn_agent";
pub const SEND_TO_AGENT: &str = "send_to_agent";
pub const WAIT_FOR_AGENT: &str = "wait_for_agent";
pub const CLOSE_AGENT: &str = "close_agent";
pub const MANAGE_ROLES: &str = "manage_roles";

// ── Integration tools ──
pub const GMAIL: &str = "gmail";
pub const GOOGLE_CALENDAR: &str = "google_calendar";
pub const NOTION: &str = "notion";
pub const LINEAR: &str = "linear";

/// Built-in tool names that external tool sources (MCP servers, WASM plugins)
/// may not shadow. A registered tool whose name collides with one of these is
/// filtered out at definition time and rejected at dispatch, so the LLM never
/// sees two competing schemas for the same name.
///
/// This is the single source of truth — both [`crate::mcp_clients`] and
/// [`crate::wasm_plugins`] reference it, so adding a new built-in tool only
/// requires extending this list (and is guarded by
/// `builtin_tool_names_includes_dispatcher_critical_names`).
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    WRITE_MEMORY,
    READ_MEMORY,
    MEMORY_SEARCH,
    LIST,
    APPLY_PATCH,
    RUN_SHELL,
    READ_FILE,
    LIST_DIR,
    WEB_FETCH,
    WEB_SEARCH,
    SCHEDULE,
    MANAGE_TASKS,
    MANAGE_CRON,
    BROWSER,
    GENERATE_IMAGE,
    TEXT_TO_SPEECH,
    TRANSCRIBE_AUDIO,
    REQUEST_USER_INPUT,
    APPLY_SKILL_PATCH,
    CREATE_CHANNEL,
    LIST_SKILLS,
    LIST_CHANNELS,
    LIST_AGENTS,
    SEARCH_TOOLS,
    SPAWN_AGENT,
    SEND_TO_AGENT,
    WAIT_FOR_AGENT,
    CLOSE_AGENT,
    MANAGE_ROLES,
    GMAIL,
    GOOGLE_CALENDAR,
    NOTION,
    LINEAR,
];

/// Helper macro for action-based tool dispatch.
///
/// Extracts the `"action"` parameter and dispatches to the matching handler.
/// Returns a user-friendly error for unknown actions.
///
/// Usage:
/// ```ignore
/// dispatch_action!(args, {
///     "create" => handle_create(args, db),
///     "list" => handle_list(args, db),
/// })
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! dispatch_action {
    ($args:expr, { $($action:literal => $handler:expr),+ $(,)? }) => {{
        let action = $crate::tool_handlers::require_str_param($args, "action")?;
        match action {
            $( $action => $handler, )+
            other => Ok(format!(
                "Unknown action: '{}'. Valid actions: {}",
                other,
                [$($action),+].join(", ")
            )),
        }
    }};
}
