//! Tool catalog: maps every tool to a group and supports profile-based filtering.
//!
//! Each built-in tool belongs to exactly one `ToolGroup`. `ToolProfile` selects
//! which groups are included when building the tool definitions sent to the LLM.

use std::collections::HashSet;

use crate::types::ToolDefinition;

/// Logical grouping of tools by purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolGroup {
    /// Memory read/write tools.
    Memory,
    /// Filesystem tools (patch, read, list).
    Fs,
    /// Shell execution tools.
    Runtime,
    /// Resource listing/discovery tools.
    Discovery,
    /// Web fetch and search tools.
    Web,
    /// Browser automation tools.
    Ui,
    /// Task and cron scheduling tools.
    Scheduling,
    /// Image generation tools.
    Media,
    /// Third-party service integrations (Gmail, Notion, etc.).
    Integration,
    /// Multi-agent orchestration tools.
    Agents,
}

/// Primary tools per group — derived from the typed [`crate::tool::Tool`]
/// enum (the single source of truth) plus the integration-tool names that
/// the agent loop doesn't dispatch directly but the catalog still filters on.
///
/// Aliases (e.g. `apply_skill_patch`, `manage_tasks`, `list_skills`) are NOT
/// listed as primary; they resolve to the same group via [`tool_group`] but
/// are dispatched through their own typed [`crate::tool::Tool`] variants.
const PRIMARY_TOOLS: &[(&str, ToolGroup)] = &[
    // Memory
    (crate::tool_names::WRITE_MEMORY, ToolGroup::Memory),
    (crate::tool_names::READ_MEMORY, ToolGroup::Memory),
    (crate::tool_names::MEMORY_SEARCH, ToolGroup::Memory),
    // Filesystem
    (crate::tool_names::APPLY_PATCH, ToolGroup::Fs),
    (crate::tool_names::READ_FILE, ToolGroup::Fs),
    (crate::tool_names::LIST_DIR, ToolGroup::Fs),
    // Runtime
    (crate::tool_names::RUN_SHELL, ToolGroup::Runtime),
    // Discovery
    (crate::tool_names::LIST, ToolGroup::Discovery),
    (crate::tool_names::REQUEST_USER_INPUT, ToolGroup::Discovery),
    // Web
    (crate::tool_names::WEB_FETCH, ToolGroup::Web),
    (crate::tool_names::WEB_SEARCH, ToolGroup::Web),
    // UI
    (crate::tool_names::BROWSER, ToolGroup::Ui),
    // Scheduling
    (crate::tool_names::SCHEDULE, ToolGroup::Scheduling),
    // Media
    (crate::tool_names::GENERATE_IMAGE, ToolGroup::Media),
    (crate::tool_names::TEXT_TO_SPEECH, ToolGroup::Media),
    // Integration (skill-driven; not dispatched by the agent loop directly)
    (crate::tool_names::GMAIL, ToolGroup::Integration),
    (crate::tool_names::GOOGLE_CALENDAR, ToolGroup::Integration),
    (crate::tool_names::NOTION, ToolGroup::Integration),
    (crate::tool_names::LINEAR, ToolGroup::Integration),
    // Agents
    (crate::tool_names::SPAWN_AGENT, ToolGroup::Agents),
    (crate::tool_names::SEND_TO_AGENT, ToolGroup::Agents),
    (crate::tool_names::WAIT_FOR_AGENT, ToolGroup::Agents),
    (crate::tool_names::CLOSE_AGENT, ToolGroup::Agents),
    (crate::tool_names::MANAGE_ROLES, ToolGroup::Agents),
];

impl ToolGroup {
    /// Parse a group name from a string (case-insensitive).
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "fs" => Some(Self::Fs),
            "runtime" => Some(Self::Runtime),
            "discovery" => Some(Self::Discovery),
            "web" => Some(Self::Web),
            "ui" => Some(Self::Ui),
            "scheduling" => Some(Self::Scheduling),
            "media" => Some(Self::Media),
            "integration" => Some(Self::Integration),
            "agents" => Some(Self::Agents),
            _ => None,
        }
    }

    /// Human-readable label for this group.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Memory => "Memory",
            Self::Fs => "Filesystem",
            Self::Runtime => "Runtime",
            Self::Discovery => "Discovery",
            Self::Web => "Web",
            Self::Ui => "UI",
            Self::Scheduling => "Scheduling",
            Self::Media => "Media",
            Self::Integration => "Integration",
            Self::Agents => "Agents",
        }
    }

    /// All primary (non-alias) tool names that belong to this group.
    pub fn tool_names(&self) -> Vec<&'static str> {
        PRIMARY_TOOLS
            .iter()
            .filter(|(_, group)| group == self)
            .map(|(name, _)| *name)
            .collect()
    }

    /// Whether this group is always included regardless of message content.
    pub fn is_core(&self) -> bool {
        matches!(
            self,
            Self::Memory | Self::Fs | Self::Runtime | Self::Discovery
        )
    }

    /// Keyword hints that trigger inclusion of this group when found in the
    /// user message. Core groups return an empty slice (always included).
    pub fn keyword_hints(&self) -> &'static [&'static str] {
        match self {
            Self::Memory | Self::Fs | Self::Runtime | Self::Discovery => &[],
            Self::Web => &[
                "search", "fetch", "url", "website", "http", "link", "web", "scrape",
            ],
            Self::Ui => &[
                "browser",
                "screenshot",
                "click",
                "navigate",
                "webpage",
                "dom",
                "scrape",
                "open page",
            ],
            Self::Scheduling => &[
                "schedule",
                "cron",
                "remind",
                "recurring",
                "every day",
                "weekly",
                "timer",
                "alarm",
            ],
            Self::Media => &[
                "image",
                "generate image",
                "picture",
                "draw",
                "photo",
                "illustration",
            ],
            Self::Integration => &[
                "email", "gmail", "calendar", "notion", "linear", "slack", "discord",
            ],
            Self::Agents => &[
                "agent",
                "spawn",
                "delegate",
                "parallel",
                "background task",
                "sub-agent",
                "subagent",
            ],
        }
    }
}

/// Filter tool definitions to only include groups relevant to the given user
/// message and recently-used tool names. Core groups are always included.
///
/// `recent_tool_names` should contain tool names used in the last few turns.
pub fn filter_tools_by_relevance(
    tools: Vec<ToolDefinition>,
    user_message: &str,
    recent_tool_names: &HashSet<String>,
    profile_groups: &HashSet<ToolGroup>,
) -> Vec<ToolDefinition> {
    let msg_lower = user_message.to_lowercase();

    // Determine which conditional groups are relevant
    let mut active_groups: HashSet<ToolGroup> =
        ALL_GROUPS.iter().filter(|g| g.is_core()).copied().collect();

    for group in ALL_GROUPS {
        if group.is_core() || !profile_groups.contains(group) {
            continue;
        }
        // Check keyword hints
        let keyword_matched = group
            .keyword_hints()
            .iter()
            .any(|hint| msg_lower.contains(hint));
        if keyword_matched {
            active_groups.insert(*group);
            continue;
        }
        // Check if any tool from this group was recently used
        let group_tools: Vec<&str> = group.tool_names();
        if group_tools
            .iter()
            .any(|name| recent_tool_names.contains(*name))
        {
            active_groups.insert(*group);
        }
    }

    // Build set of allowed tool names from active groups (primary names plus
    // their aliases — both resolve through [`tool_group`]).
    let allowed: HashSet<&str> = PRIMARY_TOOLS
        .iter()
        .filter(|(_, group)| active_groups.contains(group))
        .map(|(name, _)| *name)
        .chain(ALIAS_TOOLS.iter().filter_map(|(name, group)| {
            if active_groups.contains(group) {
                Some(*name)
            } else {
                None
            }
        }))
        .collect();

    tools
        .into_iter()
        .filter(|t| {
            let name = t.function.name.as_str();
            // Always keep tools not in the registry (user-created / dynamic)
            tool_group(name).is_none_or(|_| allowed.contains(name))
        })
        .collect()
}

/// Predefined profiles that select which tool groups are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolProfile {
    /// Only memory and discovery tools.
    Minimal,
    /// Tools for software development workflows.
    Coding,
    /// Tools for messaging and communication workflows.
    Messaging,
    /// All available tool groups enabled.
    #[default]
    Full,
}

impl ToolProfile {
    /// Parse a profile name from a string (case-insensitive).
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "coding" => Some(Self::Coding),
            "messaging" => Some(Self::Messaging),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The set of groups included in this profile.
    pub fn groups(&self) -> HashSet<ToolGroup> {
        match self {
            Self::Minimal => [ToolGroup::Memory, ToolGroup::Discovery]
                .into_iter()
                .collect(),
            Self::Coding => [
                ToolGroup::Memory,
                ToolGroup::Fs,
                ToolGroup::Runtime,
                ToolGroup::Discovery,
                ToolGroup::Web,
                ToolGroup::Media,
                ToolGroup::Scheduling,
            ]
            .into_iter()
            .collect(),
            Self::Messaging => [
                ToolGroup::Memory,
                ToolGroup::Discovery,
                ToolGroup::Integration,
                ToolGroup::Scheduling,
            ]
            .into_iter()
            .collect(),
            Self::Full => [
                ToolGroup::Memory,
                ToolGroup::Fs,
                ToolGroup::Runtime,
                ToolGroup::Discovery,
                ToolGroup::Web,
                ToolGroup::Ui,
                ToolGroup::Scheduling,
                ToolGroup::Media,
                ToolGroup::Integration,
                ToolGroup::Agents,
            ]
            .into_iter()
            .collect(),
        }
    }
}

/// All tool groups in display order.
pub const ALL_GROUPS: &[ToolGroup] = &[
    ToolGroup::Memory,
    ToolGroup::Fs,
    ToolGroup::Runtime,
    ToolGroup::Discovery,
    ToolGroup::Web,
    ToolGroup::Ui,
    ToolGroup::Scheduling,
    ToolGroup::Media,
    ToolGroup::Integration,
    ToolGroup::Agents,
];

/// Aliases — tools that share a group with a primary tool but are not listed
/// as primary themselves. Aliases are dispatched through their own typed
/// [`crate::tool::Tool`] variants but participate in catalog filtering.
const ALIAS_TOOLS: &[(&str, ToolGroup)] = &[
    (crate::tool_names::APPLY_SKILL_PATCH, ToolGroup::Fs),
    (crate::tool_names::CREATE_CHANNEL, ToolGroup::Fs),
    (crate::tool_names::LIST_SKILLS, ToolGroup::Discovery),
    (crate::tool_names::LIST_CHANNELS, ToolGroup::Discovery),
    (crate::tool_names::LIST_AGENTS, ToolGroup::Discovery),
    (crate::tool_names::MANAGE_TASKS, ToolGroup::Scheduling),
    (crate::tool_names::MANAGE_CRON, ToolGroup::Scheduling),
];

/// Map a tool name to its group. Returns `None` for user-created tools.
pub fn tool_group(name: &str) -> Option<ToolGroup> {
    PRIMARY_TOOLS
        .iter()
        .chain(ALIAS_TOOLS.iter())
        .find(|(n, _)| *n == name)
        .map(|(_, group)| *group)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_full_includes_all_groups() {
        let groups = ToolProfile::Full.groups();
        assert!(groups.contains(&ToolGroup::Memory));
        assert!(groups.contains(&ToolGroup::Fs));
        assert!(groups.contains(&ToolGroup::Runtime));
        assert!(groups.contains(&ToolGroup::Web));
        assert!(groups.contains(&ToolGroup::Ui));
        assert!(groups.contains(&ToolGroup::Integration));
        assert!(groups.contains(&ToolGroup::Agents));
    }

    #[test]
    fn profile_minimal_only_memory_and_discovery() {
        let groups = ToolProfile::Minimal.groups();
        assert_eq!(groups.len(), 2);
        assert!(groups.contains(&ToolGroup::Memory));
        assert!(groups.contains(&ToolGroup::Discovery));
    }

    #[test]
    fn profile_coding_excludes_integration_and_agents() {
        let groups = ToolProfile::Coding.groups();
        assert!(!groups.contains(&ToolGroup::Integration));
        assert!(!groups.contains(&ToolGroup::Agents));
        assert!(!groups.contains(&ToolGroup::Ui));
    }

    #[test]
    fn tool_group_maps_correctly() {
        assert_eq!(tool_group("write_memory"), Some(ToolGroup::Memory));
        assert_eq!(tool_group("apply_patch"), Some(ToolGroup::Fs));
        assert_eq!(tool_group("run_shell"), Some(ToolGroup::Runtime));
        assert_eq!(tool_group("gmail"), Some(ToolGroup::Integration));
        assert_eq!(tool_group("my_custom_tool"), None);
    }

    #[test]
    fn tool_group_aliases_map_to_fs() {
        assert_eq!(tool_group("apply_skill_patch"), Some(ToolGroup::Fs));
        assert_eq!(tool_group("create_channel"), Some(ToolGroup::Fs));
        assert_eq!(tool_group("list_dir"), Some(ToolGroup::Fs));
    }

    #[test]
    fn tool_group_aliases_map_to_discovery() {
        assert_eq!(tool_group("list_skills"), Some(ToolGroup::Discovery));
        assert_eq!(tool_group("list_channels"), Some(ToolGroup::Discovery));
    }

    #[test]
    fn profile_from_str_round_trip() {
        assert_eq!(
            ToolProfile::from_str_opt("minimal"),
            Some(ToolProfile::Minimal)
        );
        assert_eq!(
            ToolProfile::from_str_opt("coding"),
            Some(ToolProfile::Coding)
        );
        assert_eq!(
            ToolProfile::from_str_opt("messaging"),
            Some(ToolProfile::Messaging)
        );
        assert_eq!(ToolProfile::from_str_opt("full"), Some(ToolProfile::Full));
        assert_eq!(ToolProfile::from_str_opt("FULL"), Some(ToolProfile::Full));
        assert_eq!(ToolProfile::from_str_opt("unknown"), None);
    }

    #[test]
    fn group_from_str_round_trip() {
        assert_eq!(ToolGroup::from_str_opt("memory"), Some(ToolGroup::Memory));
        assert_eq!(ToolGroup::from_str_opt("fs"), Some(ToolGroup::Fs));
        assert_eq!(ToolGroup::from_str_opt("RUNTIME"), Some(ToolGroup::Runtime));
        assert_eq!(ToolGroup::from_str_opt("bogus"), None);
    }

    #[test]
    fn group_tool_names_non_empty() {
        let all_groups = [
            ToolGroup::Memory,
            ToolGroup::Fs,
            ToolGroup::Runtime,
            ToolGroup::Discovery,
            ToolGroup::Web,
            ToolGroup::Ui,
            ToolGroup::Scheduling,
            ToolGroup::Media,
            ToolGroup::Integration,
            ToolGroup::Agents,
        ];
        for g in all_groups {
            assert!(!g.tool_names().is_empty(), "{g:?} should have tool names");
        }
    }

    #[test]
    fn profile_messaging_includes_integration() {
        let groups = ToolProfile::Messaging.groups();
        assert!(groups.contains(&ToolGroup::Integration));
        assert!(groups.contains(&ToolGroup::Memory));
        assert!(groups.contains(&ToolGroup::Discovery));
        assert!(groups.contains(&ToolGroup::Scheduling));
        assert!(!groups.contains(&ToolGroup::Fs));
        assert!(!groups.contains(&ToolGroup::Runtime));
        assert!(!groups.contains(&ToolGroup::Web));
    }

    #[test]
    fn profile_coding_includes_expected_groups() {
        let groups = ToolProfile::Coding.groups();
        assert!(groups.contains(&ToolGroup::Memory));
        assert!(groups.contains(&ToolGroup::Fs));
        assert!(groups.contains(&ToolGroup::Runtime));
        assert!(groups.contains(&ToolGroup::Discovery));
        assert!(groups.contains(&ToolGroup::Web));
        assert!(groups.contains(&ToolGroup::Media));
        assert!(groups.contains(&ToolGroup::Scheduling));
        assert_eq!(groups.len(), 7);
    }

    #[test]
    fn profile_full_includes_all_10_groups() {
        let groups = ToolProfile::Full.groups();
        assert_eq!(groups.len(), 10);
    }

    #[test]
    fn tool_group_web_tools() {
        assert_eq!(tool_group("web_fetch"), Some(ToolGroup::Web));
        assert_eq!(tool_group("web_search"), Some(ToolGroup::Web));
    }

    #[test]
    fn tool_group_agent_tools() {
        assert_eq!(tool_group("spawn_agent"), Some(ToolGroup::Agents));
        assert_eq!(tool_group("send_to_agent"), Some(ToolGroup::Agents));
        assert_eq!(tool_group("wait_for_agent"), Some(ToolGroup::Agents));
        assert_eq!(tool_group("close_agent"), Some(ToolGroup::Agents));
        assert_eq!(tool_group("manage_roles"), Some(ToolGroup::Agents));
    }

    #[test]
    fn tool_group_integration_tools() {
        assert_eq!(tool_group("gmail"), Some(ToolGroup::Integration));
        assert_eq!(tool_group("google_calendar"), Some(ToolGroup::Integration));
        assert_eq!(tool_group("notion"), Some(ToolGroup::Integration));
        assert_eq!(tool_group("linear"), Some(ToolGroup::Integration));
    }

    #[test]
    fn tool_group_singleton_tools() {
        assert_eq!(tool_group("browser"), Some(ToolGroup::Ui));
        assert_eq!(tool_group("schedule"), Some(ToolGroup::Scheduling));
        assert_eq!(tool_group("generate_image"), Some(ToolGroup::Media));
    }

    #[test]
    fn group_from_str_all_variants() {
        let variants = [
            ("memory", ToolGroup::Memory),
            ("fs", ToolGroup::Fs),
            ("runtime", ToolGroup::Runtime),
            ("discovery", ToolGroup::Discovery),
            ("web", ToolGroup::Web),
            ("ui", ToolGroup::Ui),
            ("scheduling", ToolGroup::Scheduling),
            ("media", ToolGroup::Media),
            ("integration", ToolGroup::Integration),
            ("agents", ToolGroup::Agents),
        ];
        for (s, expected) in variants {
            assert_eq!(ToolGroup::from_str_opt(s), Some(expected), "failed for {s}");
        }
    }

    #[test]
    fn tool_group_names_match_group_mapping() {
        // Every tool name returned by a group's tool_names() should map back to that group
        let all_groups = [
            ToolGroup::Memory,
            ToolGroup::Fs,
            ToolGroup::Runtime,
            ToolGroup::Discovery,
            ToolGroup::Web,
            ToolGroup::Ui,
            ToolGroup::Scheduling,
            ToolGroup::Media,
            ToolGroup::Integration,
            ToolGroup::Agents,
        ];
        for group in all_groups {
            for name in group.tool_names() {
                assert_eq!(
                    tool_group(name),
                    Some(group),
                    "tool_names() for {group:?} lists '{name}' but tool_group maps it differently"
                );
            }
        }
    }

    #[test]
    fn default_profile_is_full() {
        assert_eq!(ToolProfile::default(), ToolProfile::Full);
    }

    #[test]
    fn all_groups_constant_has_all_variants() {
        assert_eq!(ALL_GROUPS.len(), 10);
        assert!(ALL_GROUPS.contains(&ToolGroup::Memory));
        assert!(ALL_GROUPS.contains(&ToolGroup::Fs));
        assert!(ALL_GROUPS.contains(&ToolGroup::Runtime));
        assert!(ALL_GROUPS.contains(&ToolGroup::Discovery));
        assert!(ALL_GROUPS.contains(&ToolGroup::Web));
        assert!(ALL_GROUPS.contains(&ToolGroup::Ui));
        assert!(ALL_GROUPS.contains(&ToolGroup::Scheduling));
        assert!(ALL_GROUPS.contains(&ToolGroup::Media));
        assert!(ALL_GROUPS.contains(&ToolGroup::Integration));
        assert!(ALL_GROUPS.contains(&ToolGroup::Agents));
    }

    #[test]
    fn group_labels_are_non_empty() {
        for group in ALL_GROUPS {
            assert!(!group.label().is_empty(), "{group:?} should have a label");
        }
    }

    #[test]
    fn group_labels_are_unique() {
        let labels: Vec<&str> = ALL_GROUPS.iter().map(|g| g.label()).collect();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len(), "group labels should be unique");
    }

    #[test]
    fn keyword_hints_cover_all_conditional_groups() {
        for group in ALL_GROUPS {
            if group.is_core() {
                assert!(
                    group.keyword_hints().is_empty(),
                    "{group:?} is core and should have no hints"
                );
            } else {
                assert!(
                    !group.keyword_hints().is_empty(),
                    "{group:?} is conditional and must have keyword hints"
                );
            }
        }
    }

    #[test]
    fn is_core_returns_expected() {
        assert!(ToolGroup::Memory.is_core());
        assert!(ToolGroup::Fs.is_core());
        assert!(ToolGroup::Runtime.is_core());
        assert!(ToolGroup::Discovery.is_core());
        assert!(!ToolGroup::Web.is_core());
        assert!(!ToolGroup::Ui.is_core());
        assert!(!ToolGroup::Scheduling.is_core());
        assert!(!ToolGroup::Media.is_core());
        assert!(!ToolGroup::Integration.is_core());
        assert!(!ToolGroup::Agents.is_core());
    }

    #[test]
    fn filter_tools_excludes_unused_groups() {
        let tools = make_test_tools();
        let profile_groups = ToolProfile::Full.groups();
        let recent = HashSet::new();

        let filtered =
            filter_tools_by_relevance(tools, "fix the bug in main.rs", &recent, &profile_groups);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();

        // Core tools always present
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"run_shell"));
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"list"));

        // Conditional tools NOT present (no keywords match)
        assert!(!names.contains(&"browser"));
        assert!(!names.contains(&"generate_image"));
        assert!(!names.contains(&"gmail"));
        assert!(!names.contains(&"spawn_agent"));
    }

    #[test]
    fn filter_tools_includes_keyword_match() {
        let tools = make_test_tools();
        let profile_groups = ToolProfile::Full.groups();
        let recent = HashSet::new();

        let filtered = filter_tools_by_relevance(
            tools,
            "open the browser and navigate",
            &recent,
            &profile_groups,
        );
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();

        assert!(
            names.contains(&"browser"),
            "browser should be included via keyword"
        );
    }

    #[test]
    fn filter_tools_includes_recently_used() {
        let tools = make_test_tools();
        let profile_groups = ToolProfile::Full.groups();
        let mut recent = HashSet::new();
        recent.insert("gmail".to_string());

        let filtered = filter_tools_by_relevance(tools, "fix the bug", &recent, &profile_groups);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();

        assert!(
            names.contains(&"gmail"),
            "gmail should be included via recent usage"
        );
    }

    #[test]
    fn filter_tools_always_includes_core() {
        let tools = make_test_tools();
        let profile_groups = ToolProfile::Full.groups();
        let recent = HashSet::new();

        let filtered = filter_tools_by_relevance(tools, "", &recent, &profile_groups);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();

        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"run_shell"));
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"list"));
    }

    #[test]
    fn filter_tools_preserves_unknown_tools() {
        // User-created / dynamic tools not in the registry should pass through
        let mut tools = make_test_tools();
        tools.push(ToolDefinition::new(
            "my_custom_tool",
            "Custom tool",
            serde_json::json!({"type":"object","properties":{}}),
        ));
        let profile_groups = ToolProfile::Full.groups();
        let recent = HashSet::new();

        let filtered = filter_tools_by_relevance(tools, "fix the bug", &recent, &profile_groups);
        let names: Vec<&str> = filtered.iter().map(|t| t.function.name.as_str()).collect();

        assert!(
            names.contains(&"my_custom_tool"),
            "unknown tools should always pass through"
        );
    }

    /// Helper to build a representative set of test tools.
    fn make_test_tools() -> Vec<ToolDefinition> {
        let schema = serde_json::json!({"type":"object","properties":{}});
        vec![
            ToolDefinition::new("write_memory", "Write memory", schema.clone()),
            ToolDefinition::new("read_memory", "Read memory", schema.clone()),
            ToolDefinition::new("apply_patch", "Apply patch", schema.clone()),
            ToolDefinition::new("read_file", "Read file", schema.clone()),
            ToolDefinition::new("list_dir", "List dir", schema.clone()),
            ToolDefinition::new("run_shell", "Run shell", schema.clone()),
            ToolDefinition::new("list", "List resources", schema.clone()),
            ToolDefinition::new("web_fetch", "Fetch URL", schema.clone()),
            ToolDefinition::new("web_search", "Search web", schema.clone()),
            ToolDefinition::new("browser", "Browser", schema.clone()),
            ToolDefinition::new("schedule", "Schedule", schema.clone()),
            ToolDefinition::new("generate_image", "Generate image", schema.clone()),
            ToolDefinition::new("gmail", "Gmail", schema.clone()),
            ToolDefinition::new("spawn_agent", "Spawn agent", schema.clone()),
        ]
    }

    /// Every tool name constant in tool_names must appear in the catalog
    /// (primary or alias). Drift means the LLM sees a tool the catalog can't
    /// classify — and vice versa.
    #[test]
    fn tool_name_constants_match_catalog() {
        use crate::tool_names;

        let catalog_names: std::collections::HashSet<&str> = PRIMARY_TOOLS
            .iter()
            .chain(ALIAS_TOOLS.iter())
            .map(|(name, _)| *name)
            .collect();

        let constants = [
            tool_names::WRITE_MEMORY,
            tool_names::READ_MEMORY,
            tool_names::MEMORY_SEARCH,
            tool_names::LIST,
            tool_names::APPLY_PATCH,
            tool_names::RUN_SHELL,
            tool_names::READ_FILE,
            tool_names::LIST_DIR,
            tool_names::WEB_FETCH,
            tool_names::WEB_SEARCH,
            tool_names::SCHEDULE,
            tool_names::MANAGE_TASKS,
            tool_names::MANAGE_CRON,
            tool_names::BROWSER,
            tool_names::GENERATE_IMAGE,
            tool_names::TEXT_TO_SPEECH,
            tool_names::REQUEST_USER_INPUT,
            tool_names::APPLY_SKILL_PATCH,
            tool_names::CREATE_CHANNEL,
            tool_names::LIST_SKILLS,
            tool_names::LIST_CHANNELS,
            tool_names::LIST_AGENTS,
            tool_names::SPAWN_AGENT,
            tool_names::SEND_TO_AGENT,
            tool_names::WAIT_FOR_AGENT,
            tool_names::CLOSE_AGENT,
            tool_names::MANAGE_ROLES,
            tool_names::GMAIL,
            tool_names::GOOGLE_CALENDAR,
            tool_names::NOTION,
            tool_names::LINEAR,
        ];

        for name in &constants {
            assert!(
                catalog_names.contains(name),
                "tool_names constant '{name}' not found in catalog"
            );
        }

        // Also verify every catalog entry has a matching constant
        for name in &catalog_names {
            assert!(
                constants.contains(name),
                "catalog entry '{name}' has no matching tool_names constant"
            );
        }
    }
}
