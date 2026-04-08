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

/// Single source of truth for tool-to-group mapping.
///
/// Each entry is `(tool_name, group, is_alias)`. Primary tools (`is_alias = false`)
/// are returned by `ToolGroup::tool_names()`. Aliases map to the same group in
/// `tool_group()` but are not listed as primary tools.
const TOOL_REGISTRY: &[(&str, ToolGroup, bool)] = &[
    // Memory
    ("write_memory", ToolGroup::Memory, false),
    ("read_memory", ToolGroup::Memory, false),
    ("memory_search", ToolGroup::Memory, false),
    // Filesystem
    ("apply_patch", ToolGroup::Fs, false),
    ("read_file", ToolGroup::Fs, false),
    ("list_dir", ToolGroup::Fs, false),
    ("apply_skill_patch", ToolGroup::Fs, true),
    ("create_channel", ToolGroup::Fs, true),
    // Runtime
    ("run_shell", ToolGroup::Runtime, false),
    // Discovery
    ("list", ToolGroup::Discovery, false),
    ("projects", ToolGroup::Discovery, false),
    ("request_user_input", ToolGroup::Discovery, false),
    ("list_skills", ToolGroup::Discovery, true),
    ("list_channels", ToolGroup::Discovery, true),
    ("list_agents", ToolGroup::Discovery, true),
    // Web
    ("web_fetch", ToolGroup::Web, false),
    ("web_search", ToolGroup::Web, false),
    // UI
    ("browser", ToolGroup::Ui, false),
    // Scheduling
    ("schedule", ToolGroup::Scheduling, false),
    ("manage_tasks", ToolGroup::Scheduling, true),
    ("manage_cron", ToolGroup::Scheduling, true),
    // Media
    ("generate_image", ToolGroup::Media, false),
    ("text_to_speech", ToolGroup::Media, false),
    // Integration
    ("gmail", ToolGroup::Integration, false),
    ("google_calendar", ToolGroup::Integration, false),
    ("notion", ToolGroup::Integration, false),
    ("linear", ToolGroup::Integration, false),
    // Agents
    ("spawn_agent", ToolGroup::Agents, false),
    ("send_to_agent", ToolGroup::Agents, false),
    ("wait_for_agent", ToolGroup::Agents, false),
    ("close_agent", ToolGroup::Agents, false),
    ("manage_roles", ToolGroup::Agents, false),
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
        TOOL_REGISTRY
            .iter()
            .filter(|(_, group, is_alias)| group == self && !is_alias)
            .map(|(name, _, _)| *name)
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

    // Build set of allowed tool names from active groups
    let allowed: HashSet<&str> = TOOL_REGISTRY
        .iter()
        .filter(|(_, group, _)| active_groups.contains(group))
        .map(|(name, _, _)| *name)
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

/// Map a tool name to its group. Returns `None` for user-created tools.
pub fn tool_group(name: &str) -> Option<ToolGroup> {
    TOOL_REGISTRY
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, group, _)| *group)
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
}
