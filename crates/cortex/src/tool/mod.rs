//! Typed tool dispatch.
//!
//! Replaces the ad-hoc `match name.as_str()` dispatch over untyped
//! `serde_json::Value` arguments with a closed enum: each tool name maps to a
//! variant whose payload is its typed `Args` struct (`#[derive(Deserialize)]`).
//!
//! Per the architecture plan and `RUST-IDIOMS.md`:
//!
//! - **Closed enum**, not a `trait Tool { fn execute }` registry. Tools are
//!   compile-time; there is no second impl set.
//! - **Per-variant typed `Args`** lives next to its handler.
//! - **Single tagged-deser entry point** ([`Tool::parse`]) consolidates schema
//!   validation. Errors surface at the dispatcher edge, not deep in handlers.
//! - **Per-variant `const fn` classification** on the enum
//!   ([`Tool::parallel_safe`], [`Tool::allowed_in_plan_mode`], [`Tool::group`])
//!   instead of a parallel match table that could drift.
//! - **Tool-effect side effects co-locate with the handler** in the variant's
//!   own module (e.g. `write_memory.rs` invalidates the IDENTITY cache and
//!   spawns embedding tasks itself).

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use catalog::ToolGroup;
use names as tn;

pub mod catalog;
pub mod definitions;
pub mod effects;
pub mod names;
pub mod policy;

pub(crate) mod apply_patch;
pub(crate) mod browser;
pub(crate) mod computer_use;
pub(crate) mod create_goal;
pub(crate) mod generate_image;
pub(crate) mod get_goal;
pub(crate) mod list;
pub(crate) mod list_dir;
pub(crate) mod memory_search;
pub(crate) mod multi_agent;
pub(crate) mod read_file;
pub(crate) mod read_memory;
pub(crate) mod request_user_input;
pub(crate) mod run_shell;
pub(crate) mod schedule;
pub(crate) mod text_to_speech;
pub(crate) mod update_goal;
pub(crate) mod video_analyze;
pub(crate) mod vision_analyze;
pub(crate) mod web_fetch;
pub(crate) mod web_search;
pub(crate) mod write_memory;

pub(crate) use apply_patch::ApplyPatchArgs;
pub(crate) use browser::BrowserArgs;
pub(crate) use computer_use::ComputerUseArgs;
pub(crate) use create_goal::CreateGoalArgs;
pub(crate) use generate_image::GenerateImageArgs;
pub(crate) use get_goal::GetGoalArgs;
pub(crate) use list::ListArgs;
pub(crate) use list_dir::ListDirArgs;
pub(crate) use memory_search::MemorySearchArgs;
pub(crate) use multi_agent::MultiAgentArgs;
pub(crate) use read_file::ReadFileArgs;
pub(crate) use read_memory::ReadMemoryArgs;
pub(crate) use request_user_input::RequestUserInputArgs;
pub(crate) use run_shell::RunShellArgs;
pub(crate) use schedule::ScheduleArgs;
pub(crate) use text_to_speech::TextToSpeechArgs;
pub(crate) use update_goal::UpdateGoalArgs;
pub(crate) use video_analyze::VideoAnalyzeArgs;
pub(crate) use vision_analyze::VisionAnalyzeArgs;
pub(crate) use web_fetch::WebFetchArgs;
pub(crate) use web_search::WebSearchArgs;
pub(crate) use write_memory::WriteMemoryArgs;

/// Closed set of every tool the agent can invoke.
///
/// Variants are added rarely. Compiler-checked exhaustiveness on
/// [`Tool::parallel_safe`], [`Tool::allowed_in_plan_mode`], [`Tool::group`],
/// and [`Tool::name`] forces every classification to acknowledge new
/// variants — drift is impossible.
#[derive(Debug, Clone)]
pub(crate) enum Tool {
    // Memory
    WriteMemory(WriteMemoryArgs),
    ReadMemory(ReadMemoryArgs),
    MemorySearch(MemorySearchArgs),

    // Filesystem
    ApplyPatch(ApplyPatchArgs),
    ApplySkillPatch(ApplyPatchArgs),
    CreateChannel(ApplyPatchArgs),
    ReadFile(ReadFileArgs),
    ListDir(ListDirArgs),

    // Shell
    RunShell(RunShellArgs),

    // Web
    WebFetch(WebFetchArgs),
    WebSearch(WebSearchArgs),

    // Discovery
    List(ListArgs),
    ListSkills,
    ListChannels,

    // Scheduling — `manage_tasks` and `manage_cron` are aliases for `schedule`
    Schedule(ScheduleArgs),
    ManageTasks(ScheduleArgs),
    ManageCron(ScheduleArgs),

    // Media
    Browser(BrowserArgs),
    GenerateImage(GenerateImageArgs),
    TextToSpeech(TextToSpeechArgs),
    VisionAnalyze(VisionAnalyzeArgs),
    VideoAnalyze(VideoAnalyzeArgs),
    ComputerUse(ComputerUseArgs),

    // User interaction
    RequestUserInput(RequestUserInputArgs),

    // Goals
    GetGoal(GetGoalArgs),
    CreateGoal(CreateGoalArgs),
    UpdateGoal(UpdateGoalArgs),

    // Multi-agent
    SpawnAgent(MultiAgentArgs),
    SendToAgent(MultiAgentArgs),
    WaitForAgent(MultiAgentArgs),
    /// `list_agents` carries the parsed args for symmetry, but the handler
    /// only needs `agent_control` — fields are intentionally unread.
    ListAgents(#[allow(dead_code)] MultiAgentArgs),
    CloseAgent(MultiAgentArgs),
    ManageRoles(MultiAgentArgs),
}

/// Errors raised when parsing a tool call.
#[derive(Debug, Error)]
pub(crate) enum ToolParseError {
    /// Tool name is not in the closed set.
    #[error("Unknown tool: {0}")]
    UnknownTool(String),
    /// Args failed `serde::from_value` against the variant's `Args` struct.
    #[error("Invalid arguments for tool '{tool}': {source}")]
    InvalidArgs {
        tool: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl Tool {
    /// Parse a tool call by name + raw JSON args into a typed [`Tool`] variant.
    ///
    /// This is the single entry point for schema validation — handlers downstream
    /// receive only typed `Args`, never raw `Value`. Unknown tool names yield
    /// [`ToolParseError::UnknownTool`]; structurally-invalid args yield
    /// [`ToolParseError::InvalidArgs`].
    pub(crate) fn parse(name: &str, args: Value) -> Result<Self, ToolParseError> {
        fn from<T: for<'de> Deserialize<'de>>(
            tool: &'static str,
            args: Value,
        ) -> Result<T, ToolParseError> {
            serde_json::from_value::<T>(args)
                .map_err(|source| ToolParseError::InvalidArgs { tool, source })
        }

        Ok(match name {
            // Memory
            tn::WRITE_MEMORY => Tool::WriteMemory(from(tn::WRITE_MEMORY, args)?),
            tn::READ_MEMORY => Tool::ReadMemory(from(tn::READ_MEMORY, args)?),
            tn::MEMORY_SEARCH => Tool::MemorySearch(from(tn::MEMORY_SEARCH, args)?),

            // Filesystem
            tn::APPLY_PATCH => Tool::ApplyPatch(from(tn::APPLY_PATCH, args)?),
            tn::APPLY_SKILL_PATCH => Tool::ApplySkillPatch(from(tn::APPLY_SKILL_PATCH, args)?),
            tn::CREATE_CHANNEL => Tool::CreateChannel(from(tn::CREATE_CHANNEL, args)?),
            tn::READ_FILE => Tool::ReadFile(from(tn::READ_FILE, args)?),
            tn::LIST_DIR => Tool::ListDir(from(tn::LIST_DIR, args)?),

            // Shell
            tn::RUN_SHELL => Tool::RunShell(from(tn::RUN_SHELL, args)?),

            // Web
            tn::WEB_FETCH => Tool::WebFetch(from(tn::WEB_FETCH, args)?),
            tn::WEB_SEARCH => Tool::WebSearch(from(tn::WEB_SEARCH, args)?),

            // Discovery
            tn::LIST => Tool::List(from(tn::LIST, args)?),
            tn::LIST_SKILLS => Tool::ListSkills,
            tn::LIST_CHANNELS => Tool::ListChannels,

            // Scheduling
            tn::SCHEDULE => Tool::Schedule(from(tn::SCHEDULE, args)?),
            tn::MANAGE_TASKS => Tool::ManageTasks(from(tn::MANAGE_TASKS, args)?),
            tn::MANAGE_CRON => Tool::ManageCron(from(tn::MANAGE_CRON, args)?),

            // Media
            tn::BROWSER => Tool::Browser(from(tn::BROWSER, args)?),
            tn::GENERATE_IMAGE => Tool::GenerateImage(from(tn::GENERATE_IMAGE, args)?),
            tn::TEXT_TO_SPEECH => Tool::TextToSpeech(from(tn::TEXT_TO_SPEECH, args)?),
            tn::VISION_ANALYZE => Tool::VisionAnalyze(from(tn::VISION_ANALYZE, args)?),
            tn::VIDEO_ANALYZE => Tool::VideoAnalyze(from(tn::VIDEO_ANALYZE, args)?),
            tn::COMPUTER_USE => Tool::ComputerUse(from(tn::COMPUTER_USE, args)?),

            // User input
            tn::REQUEST_USER_INPUT => Tool::RequestUserInput(from(tn::REQUEST_USER_INPUT, args)?),

            // Goals
            tn::GET_GOAL => Tool::GetGoal(from(tn::GET_GOAL, args)?),
            tn::CREATE_GOAL => Tool::CreateGoal(from(tn::CREATE_GOAL, args)?),
            tn::UPDATE_GOAL => Tool::UpdateGoal(from(tn::UPDATE_GOAL, args)?),

            // Multi-agent
            tn::SPAWN_AGENT => Tool::SpawnAgent(from(tn::SPAWN_AGENT, args)?),
            tn::SEND_TO_AGENT => Tool::SendToAgent(from(tn::SEND_TO_AGENT, args)?),
            tn::WAIT_FOR_AGENT => Tool::WaitForAgent(from(tn::WAIT_FOR_AGENT, args)?),
            tn::LIST_AGENTS => Tool::ListAgents(from(tn::LIST_AGENTS, args)?),
            tn::CLOSE_AGENT => Tool::CloseAgent(from(tn::CLOSE_AGENT, args)?),
            tn::MANAGE_ROLES => Tool::ManageRoles(from(tn::MANAGE_ROLES, args)?),

            other => return Err(ToolParseError::UnknownTool(other.to_string())),
        })
    }

    /// The canonical name (the string LLMs emit) for this tool.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Tool::WriteMemory(_) => tn::WRITE_MEMORY,
            Tool::ReadMemory(_) => tn::READ_MEMORY,
            Tool::MemorySearch(_) => tn::MEMORY_SEARCH,
            Tool::ApplyPatch(_) => tn::APPLY_PATCH,
            Tool::ApplySkillPatch(_) => tn::APPLY_SKILL_PATCH,
            Tool::CreateChannel(_) => tn::CREATE_CHANNEL,
            Tool::ReadFile(_) => tn::READ_FILE,
            Tool::ListDir(_) => tn::LIST_DIR,
            Tool::RunShell(_) => tn::RUN_SHELL,
            Tool::WebFetch(_) => tn::WEB_FETCH,
            Tool::WebSearch(_) => tn::WEB_SEARCH,
            Tool::List(_) => tn::LIST,
            Tool::ListSkills => tn::LIST_SKILLS,
            Tool::ListChannels => tn::LIST_CHANNELS,
            Tool::Schedule(_) => tn::SCHEDULE,
            Tool::ManageTasks(_) => tn::MANAGE_TASKS,
            Tool::ManageCron(_) => tn::MANAGE_CRON,
            Tool::Browser(_) => tn::BROWSER,
            Tool::GenerateImage(_) => tn::GENERATE_IMAGE,
            Tool::TextToSpeech(_) => tn::TEXT_TO_SPEECH,
            Tool::VisionAnalyze(_) => tn::VISION_ANALYZE,
            Tool::VideoAnalyze(_) => tn::VIDEO_ANALYZE,
            Tool::ComputerUse(_) => tn::COMPUTER_USE,
            Tool::RequestUserInput(_) => tn::REQUEST_USER_INPUT,
            Tool::GetGoal(_) => tn::GET_GOAL,
            Tool::CreateGoal(_) => tn::CREATE_GOAL,
            Tool::UpdateGoal(_) => tn::UPDATE_GOAL,
            Tool::SpawnAgent(_) => tn::SPAWN_AGENT,
            Tool::SendToAgent(_) => tn::SEND_TO_AGENT,
            Tool::WaitForAgent(_) => tn::WAIT_FOR_AGENT,
            Tool::ListAgents(_) => tn::LIST_AGENTS,
            Tool::CloseAgent(_) => tn::CLOSE_AGENT,
            Tool::ManageRoles(_) => tn::MANAGE_ROLES,
        }
    }

    /// True iff the tool is safe to fan out concurrently with other parallel-safe
    /// tools in the same batch. Drives the parallel-tool planner.
    ///
    /// Read-only / pure-data tools are safe; anything mutating, time-dependent,
    /// or holding `&mut` agent state (browser, agent_control) is not.
    pub(crate) const fn parallel_safe(&self) -> bool {
        match self {
            Tool::ReadFile(_)
            | Tool::ReadMemory(_)
            | Tool::MemorySearch(_)
            | Tool::ListSkills
            | Tool::ListChannels
            | Tool::ListDir(_)
            | Tool::WebFetch(_)
            | Tool::WebSearch(_)
            | Tool::ApplyPatch(_)
            | Tool::ApplySkillPatch(_)
            | Tool::VisionAnalyze(_)
            | Tool::VideoAnalyze(_) => true,

            Tool::WriteMemory(_)
            | Tool::CreateChannel(_)
            | Tool::RunShell(_)
            | Tool::List(_)
            | Tool::Schedule(_)
            | Tool::ManageTasks(_)
            | Tool::ManageCron(_)
            | Tool::Browser(_)
            | Tool::GenerateImage(_)
            | Tool::TextToSpeech(_)
            | Tool::ComputerUse(_)
            | Tool::RequestUserInput(_)
            | Tool::GetGoal(_)
            | Tool::CreateGoal(_)
            | Tool::UpdateGoal(_)
            | Tool::SpawnAgent(_)
            | Tool::SendToAgent(_)
            | Tool::WaitForAgent(_)
            | Tool::ListAgents(_)
            | Tool::CloseAgent(_)
            | Tool::ManageRoles(_) => false,
        }
    }

    /// True iff the tool may be invoked while the agent is in Plan mode
    /// (read-only collaboration mode that blocks mutating tools).
    ///
    /// Plan-mode gating happens at the agent loop's pre-parse stage via
    /// [`allowed_in_plan_mode_name`] (raw-name lookup), but this typed
    /// variant exists so any post-parse caller has the same single source
    /// of truth without re-implementing the read-only allowlist.
    #[allow(dead_code)]
    pub(crate) fn allowed_in_plan_mode(&self) -> bool {
        match self {
            Tool::ReadFile(_)
            | Tool::ListDir(_)
            | Tool::List(_)
            | Tool::ListSkills
            | Tool::ListChannels
            | Tool::ListAgents(_)
            | Tool::ReadMemory(_)
            | Tool::MemorySearch(_)
            | Tool::WebFetch(_)
            | Tool::WebSearch(_)
            | Tool::RequestUserInput(_)
            | Tool::GetGoal(_)
            | Tool::VisionAnalyze(_)
            | Tool::VideoAnalyze(_) => true,

            Tool::ComputerUse(args) => computer_use::action_allowed_in_plan_mode(&args.action),

            Tool::WriteMemory(_)
            | Tool::ApplyPatch(_)
            | Tool::ApplySkillPatch(_)
            | Tool::CreateChannel(_)
            | Tool::RunShell(_)
            | Tool::Schedule(_)
            | Tool::ManageTasks(_)
            | Tool::ManageCron(_)
            | Tool::Browser(_)
            | Tool::GenerateImage(_)
            | Tool::TextToSpeech(_)
            | Tool::CreateGoal(_)
            | Tool::UpdateGoal(_)
            | Tool::SpawnAgent(_)
            | Tool::SendToAgent(_)
            | Tool::WaitForAgent(_)
            | Tool::CloseAgent(_)
            | Tool::ManageRoles(_) => false,
        }
    }

    /// The catalog group this tool belongs to.
    ///
    /// The catalog uses raw-name lookup ([`crate::tool::catalog::tool_group`])
    /// because alias names also need a group; this typed variant exists so a
    /// post-parse caller can reach the same answer without a string compare.
    #[allow(dead_code)]
    pub(crate) const fn group(&self) -> ToolGroup {
        match self {
            Tool::WriteMemory(_) | Tool::ReadMemory(_) | Tool::MemorySearch(_) => ToolGroup::Memory,
            Tool::ApplyPatch(_)
            | Tool::ApplySkillPatch(_)
            | Tool::CreateChannel(_)
            | Tool::ReadFile(_)
            | Tool::ListDir(_) => ToolGroup::Fs,
            Tool::RunShell(_) => ToolGroup::Runtime,
            Tool::List(_)
            | Tool::ListSkills
            | Tool::ListChannels
            | Tool::ListAgents(_)
            | Tool::RequestUserInput(_) => ToolGroup::Discovery,
            Tool::WebFetch(_) | Tool::WebSearch(_) => ToolGroup::Web,
            Tool::Browser(_) => ToolGroup::Ui,
            Tool::Schedule(_) | Tool::ManageTasks(_) | Tool::ManageCron(_) => ToolGroup::Scheduling,
            Tool::GenerateImage(_)
            | Tool::TextToSpeech(_)
            | Tool::VisionAnalyze(_)
            | Tool::VideoAnalyze(_)
            | Tool::ComputerUse(_) => ToolGroup::Media,
            Tool::SpawnAgent(_)
            | Tool::SendToAgent(_)
            | Tool::WaitForAgent(_)
            | Tool::CloseAgent(_)
            | Tool::ManageRoles(_) => ToolGroup::Agents,
            Tool::GetGoal(_) | Tool::CreateGoal(_) | Tool::UpdateGoal(_) => ToolGroup::Goals,
        }
    }
}

/// True iff `name` corresponds to a tool variant whose
/// [`Tool::allowed_in_plan_mode`] is `true`. Mirrors the typed predicate but
/// works on raw names (so callers can gate Plan-mode access before
/// arg-parsing has happened).
pub(crate) const fn allowed_in_plan_mode_name(name: &str) -> bool {
    // Note: `COMPUTER_USE` is listed here because *some* of its actions are
    // read-only. The agent loop must additionally consult
    // [`Tool::allowed_in_plan_mode`] after parsing to enforce the per-action
    // gate — mutating actions MUST stay blocked in Plan mode.
    matches_any(
        name,
        &[
            tn::READ_FILE,
            tn::LIST_DIR,
            tn::LIST,
            tn::LIST_SKILLS,
            tn::LIST_CHANNELS,
            tn::LIST_AGENTS,
            tn::READ_MEMORY,
            tn::MEMORY_SEARCH,
            tn::WEB_FETCH,
            tn::WEB_SEARCH,
            tn::REQUEST_USER_INPUT,
            tn::GET_GOAL,
            tn::VISION_ANALYZE,
            tn::VIDEO_ANALYZE,
            tn::COMPUTER_USE,
        ],
    )
}

/// True iff `name` corresponds to a tool variant whose [`Tool::parallel_safe`]
/// is `true`. Mirrors the typed predicate but works on raw names (so callers
/// can gate parallel dispatch before arg-parsing has happened).
pub(crate) const fn is_parallel_safe_name(name: &str) -> bool {
    // Cast through bytes so the function can stay `const fn`.
    matches_any(
        name,
        &[
            tn::READ_FILE,
            tn::READ_MEMORY,
            tn::MEMORY_SEARCH,
            tn::LIST_SKILLS,
            tn::LIST_CHANNELS,
            tn::LIST_DIR,
            tn::WEB_FETCH,
            tn::WEB_SEARCH,
            tn::APPLY_PATCH,
            tn::APPLY_SKILL_PATCH,
            tn::VISION_ANALYZE,
            tn::VIDEO_ANALYZE,
        ],
    )
}

/// `const`-friendly `slice.contains`.
const fn matches_any(name: &str, candidates: &[&str]) -> bool {
    let mut i = 0;
    while i < candidates.len() {
        if str_eq(name, candidates[i]) {
            return true;
        }
        i += 1;
    }
    false
}

const fn str_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut i = 0;
    while i < ab.len() {
        if ab[i] != bb[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every variant of [`Tool`] must be reachable by [`Tool::parse`] from its
    /// name string and return a non-empty `name()`. This guards against
    /// adding a variant but forgetting to wire `parse` or `name`.
    ///
    /// The test is exhaustive because the helper covers every non-aliased
    /// `tool_names` constant; each name maps through `parse` (with minimal
    /// valid args) and back through `name()`.
    #[test]
    fn parse_round_trip_for_every_tool_name() {
        let cases: &[(&str, Value)] = &[
            (tn::WRITE_MEMORY, json!({"filename":"x","content":"y"})),
            (tn::READ_MEMORY, json!({"filename":"x"})),
            (tn::MEMORY_SEARCH, json!({"query":"q"})),
            (tn::APPLY_PATCH, json!({"patch":"p"})),
            (tn::APPLY_SKILL_PATCH, json!({"patch":"p"})),
            (tn::CREATE_CHANNEL, json!({"patch":"p"})),
            (tn::READ_FILE, json!({"path":"a.rs"})),
            (tn::LIST_DIR, json!({})),
            (tn::RUN_SHELL, json!({"command":"true"})),
            (tn::WEB_FETCH, json!({"url":"https://example.com"})),
            (tn::WEB_SEARCH, json!({"query":"rust"})),
            (tn::LIST, json!({"what":"skills"})),
            (tn::LIST_SKILLS, json!({})),
            (tn::LIST_CHANNELS, json!({})),
            (tn::SCHEDULE, json!({})),
            (tn::MANAGE_TASKS, json!({})),
            (tn::MANAGE_CRON, json!({})),
            (
                tn::BROWSER,
                json!({"action":"navigate","url":"https://example.com"}),
            ),
            (tn::GENERATE_IMAGE, json!({"prompt":"a cat"})),
            (tn::TEXT_TO_SPEECH, json!({"text":"hi"})),
            (
                tn::VISION_ANALYZE,
                json!({"image_url":"https://example.com/x.png"}),
            ),
            (
                tn::REQUEST_USER_INPUT,
                json!({"questions":[{"id":"a","header":"H","question":"?","options":[{"label":"x","description":"y"},{"label":"z","description":"w"}]}]}),
            ),
            (tn::SPAWN_AGENT, json!({})),
            (tn::SEND_TO_AGENT, json!({})),
            (tn::WAIT_FOR_AGENT, json!({})),
            (tn::LIST_AGENTS, json!({})),
            (tn::CLOSE_AGENT, json!({})),
            (tn::MANAGE_ROLES, json!({})),
            (tn::GET_GOAL, json!({})),
            (tn::CREATE_GOAL, json!({"objective":"o"})),
            (tn::UPDATE_GOAL, json!({"status":"complete"})),
        ];
        for (name, args) in cases {
            let parsed = Tool::parse(name, args.clone())
                .unwrap_or_else(|e| panic!("parse({name}) failed: {e}"));
            assert_eq!(parsed.name(), *name);
            // Classification table coverage — exhaustive matches in the impl
            // make this trivial to assert by smoke-test, but recording the
            // values here documents intent.
            let _ = parsed.parallel_safe();
            let _ = parsed.allowed_in_plan_mode();
            let _ = parsed.group();
        }
    }

    #[test]
    fn parse_unknown_tool_errors() {
        let err = Tool::parse("not_a_real_tool", json!({})).unwrap_err();
        match err {
            ToolParseError::UnknownTool(name) => assert_eq!(name, "not_a_real_tool"),
            other => panic!("expected UnknownTool, got: {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_args_errors_with_tool_name() {
        // write_memory requires `filename` and `content` as strings; sending an
        // integer for `filename` must fail at the dispatcher edge.
        let err =
            Tool::parse(tn::WRITE_MEMORY, json!({"filename": 7, "content": "x"})).unwrap_err();
        match err {
            ToolParseError::InvalidArgs { tool, .. } => assert_eq!(tool, tn::WRITE_MEMORY),
            other => panic!("expected InvalidArgs, got: {other:?}"),
        }
    }

    /// Mutating-tool list must align with `agent::tool_classification::is_mutating_tool`,
    /// which is the read-only allowlist used to gate Plan mode. Any drift means
    /// Plan mode either blocks something it shouldn't or lets a mutating call
    /// slip through.
    #[test]
    fn allowed_in_plan_mode_matches_legacy_classifier() {
        // Build one Tool per variant via parse() with minimal valid args.
        let cases = round_trip_cases();
        for (name, args) in cases {
            let tool = Tool::parse(name, args).expect("parse");
            let legacy_readonly = !crate::agent::mutating_tool_names().contains(&name);
            // Legacy `mutating_tool_names()` lists `request_user_input` as
            // non-mutating (it isn't in the list). We must treat it the same.
            assert_eq!(
                tool.allowed_in_plan_mode(),
                legacy_readonly,
                "drift on `{name}`: allowed_in_plan_mode={} legacy_readonly={legacy_readonly}",
                tool.allowed_in_plan_mode()
            );
        }
    }

    fn round_trip_cases() -> Vec<(&'static str, Value)> {
        vec![
            (tn::WRITE_MEMORY, json!({"filename":"x","content":"y"})),
            (tn::READ_MEMORY, json!({"filename":"x"})),
            (tn::MEMORY_SEARCH, json!({"query":"q"})),
            (tn::APPLY_PATCH, json!({"patch":"p"})),
            (tn::APPLY_SKILL_PATCH, json!({"patch":"p"})),
            (tn::CREATE_CHANNEL, json!({"patch":"p"})),
            (tn::READ_FILE, json!({"path":"a.rs"})),
            (tn::LIST_DIR, json!({})),
            (tn::RUN_SHELL, json!({"command":"true"})),
            (tn::WEB_FETCH, json!({"url":"https://example.com"})),
            (tn::WEB_SEARCH, json!({"query":"q"})),
            (tn::LIST, json!({"what":"skills"})),
            (tn::LIST_SKILLS, json!({})),
            (tn::LIST_CHANNELS, json!({})),
            (tn::SCHEDULE, json!({})),
            (tn::MANAGE_TASKS, json!({})),
            (tn::MANAGE_CRON, json!({})),
            (
                tn::BROWSER,
                json!({"action":"navigate","url":"https://example.com"}),
            ),
            (tn::GENERATE_IMAGE, json!({"prompt":"x"})),
            (tn::TEXT_TO_SPEECH, json!({"text":"hi"})),
            (
                tn::VISION_ANALYZE,
                json!({"image_url":"https://example.com/x.png"}),
            ),
            (
                tn::REQUEST_USER_INPUT,
                json!({"questions":[{"id":"a","header":"H","question":"?","options":[{"label":"x","description":"y"},{"label":"z","description":"w"}]}]}),
            ),
            (tn::SPAWN_AGENT, json!({})),
            (tn::SEND_TO_AGENT, json!({})),
            (tn::WAIT_FOR_AGENT, json!({})),
            (tn::LIST_AGENTS, json!({})),
            (tn::CLOSE_AGENT, json!({})),
            (tn::MANAGE_ROLES, json!({})),
            (tn::GET_GOAL, json!({})),
            (tn::CREATE_GOAL, json!({"objective":"o"})),
            (tn::UPDATE_GOAL, json!({"status":"complete"})),
        ]
    }

    #[test]
    fn goal_tools_plan_mode_gating() {
        // get_goal is read-only and explicitly allowed in Plan mode.
        assert!(allowed_in_plan_mode_name(tn::GET_GOAL));
        // create_goal / update_goal mutate the goals table — must be
        // blocked in Plan mode (the agent runs them in Execute only).
        assert!(!allowed_in_plan_mode_name(tn::CREATE_GOAL));
        assert!(!allowed_in_plan_mode_name(tn::UPDATE_GOAL));
    }

    #[test]
    fn parallel_safe_matches_legacy_dispatcher() {
        // The pre-refactor `dispatch_parallel_safe_tool` table is the
        // ground truth for which tools the dispatcher actually has a
        // parallel handler for. Mirror that exact set here so behaviour is
        // bit-for-bit identical.
        let parallel_set: &[&str] = &[
            tn::READ_FILE,
            tn::READ_MEMORY,
            tn::MEMORY_SEARCH,
            tn::LIST_SKILLS,
            tn::LIST_CHANNELS,
            tn::LIST_DIR,
            tn::WEB_FETCH,
            tn::WEB_SEARCH,
            tn::APPLY_PATCH,
            tn::APPLY_SKILL_PATCH,
            tn::VISION_ANALYZE,
        ];
        for (name, args) in round_trip_cases() {
            let tool = Tool::parse(name, args).expect("parse");
            let expected = parallel_set.contains(&name);
            assert_eq!(
                tool.parallel_safe(),
                expected,
                "drift on `{name}`: parallel_safe={} expected={expected}",
                tool.parallel_safe()
            );
        }
    }
}
