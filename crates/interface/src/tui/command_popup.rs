use super::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::custom_commands::CustomCommand;
use super::theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandDef {
    pub name: String,
    pub description: String,
    /// `None` for built-in commands (handled by `try_handle_command`); `Some`
    /// for user-defined commands loaded from `~/.borg/commands/<name>.md` —
    /// the body is a prompt template substituted into the user message.
    pub template: Option<String>,
}

const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    // Essentials
    ("/help", "Show available commands"),
    ("/settings", "Configure settings"),
    ("/usage", "Show usage stats"),
    ("/plan", "Toggle plan mode"),
    ("/mode", "Switch collaboration mode"),
    // Conversation
    ("/compact", "Compact conversation history"),
    ("/clear", "Clear conversation"),
    ("/undo", "Undo last agent turn"),
    // Context
    ("/memory", "Show memory context"),
    ("/history", "Show conversation history"),
    ("/logs", "Show activity log"),
    // Diagnostics
    ("/doctor", "Run diagnostics"),
    ("/heal", "Run self-healing maintenance sweep"),
    ("/stats", "Show agent vitals"),
    ("/evolution", "Show evolution overview"),
    ("/card", "Print an ASCII share card"),
    ("/pairing", "Manage sender pairing"),
    (
        "/update",
        "Update borg (--prerelease for experimental, --check to dry-run)",
    ),
    // Sessions
    ("/sessions", "Browse and load saved sessions"),
    ("/save", "Save current session"),
    ("/new", "Start new session"),
    ("/fork", "Branch the current session into a new one"),
    // Integrations
    ("/plugins", "Manage plugins and channels"),
    ("/projects", "List projects"),
    ("/schedule", "Manage schedules"),
    ("/migrate", "Import from another agent"),
    ("/restart", "Restart gateway server"),
    ("/xp", "Show XP summary and feed"),
];

fn builtin_defs() -> Vec<SlashCommandDef> {
    BUILTIN_COMMANDS
        .iter()
        .map(|(name, desc)| SlashCommandDef {
            name: (*name).to_string(),
            description: (*desc).to_string(),
            template: None,
        })
        .collect()
}

/// True when every char of `needle` appears in `haystack` in order
/// (not necessarily contiguously). Both inputs are expected lowercase.
/// Return the char-indices in `haystack` that match `needle` greedily
/// left-to-right (case-insensitive). Used to underline matched characters
/// in the command popup. Empty/no-match returns an empty set.
fn subsequence_match_positions(needle: &str, haystack: &str) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    if needle.is_empty() {
        return out;
    }
    let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let mut ni = 0usize;
    for (hi, hc) in haystack.chars().enumerate() {
        if ni >= needle_lower.len() {
            break;
        }
        if hc.to_lowercase().next() == Some(needle_lower[ni]) {
            out.insert(hi);
            ni += 1;
        }
    }
    if ni < needle_lower.len() {
        out.clear();
    }
    out
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut n = needle.chars();
    let mut curr = n.next();
    for hc in haystack.chars() {
        match curr {
            Some(nc) if nc == hc => curr = n.next(),
            _ => {}
        }
        if curr.is_none() {
            return true;
        }
    }
    curr.is_none()
}

pub struct CommandPopup {
    visible: bool,
    filter: String,
    selected: usize,
    /// Built-ins followed by custom commands (in load order). Built-ins always
    /// rank first within a tier so a stray `~/.borg/commands/help.md` cannot
    /// shadow `/help`.
    commands: Vec<SlashCommandDef>,
}

impl CommandPopup {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_custom(Vec::new())
    }

    /// Build a popup whose palette includes both the built-in commands and
    /// the supplied user-defined ones. Custom commands whose name collides
    /// with a built-in are dropped.
    pub fn with_custom(custom: Vec<CustomCommand>) -> Self {
        let mut commands = builtin_defs();
        let builtin_names: std::collections::HashSet<String> =
            commands.iter().map(|c| c.name.clone()).collect();
        for c in custom {
            if builtin_names.contains(&c.name) {
                tracing::warn!(name = %c.name, "ignoring custom command: name collides with built-in");
                continue;
            }
            commands.push(SlashCommandDef {
                name: c.name,
                description: c.description,
                template: Some(c.template),
            });
        }
        Self {
            visible: false,
            filter: String::new(),
            selected: 0,
            commands,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn update_filter(&mut self, text: &str) {
        // Show popup only when text starts with '/' and has no spaces
        if text.starts_with('/') && !text.contains(' ') {
            self.filter = text[1..].to_string();
            self.visible = true;
            // Reset to top on every filter change so the best match is selected
            self.selected = 0;
        } else {
            self.visible = false;
        }
    }

    pub fn filtered(&self) -> Vec<&SlashCommandDef> {
        if self.filter.is_empty() {
            return self.commands.iter().collect();
        }

        let filter_lower = self.filter.to_lowercase();

        // Three-tier ranking: exact match > prefix match > subsequence match.
        // Within each tier, preserve declaration order so related commands
        // stay grouped (e.g. /settings near /status). Built-ins are stored
        // before custom commands, so they naturally rank first per tier.
        let mut exact = Vec::new();
        let mut prefix = Vec::new();
        let mut fuzzy = Vec::new();

        for cmd in &self.commands {
            let cmd_name = &cmd.name[1..]; // strip leading '/'
            let cmd_lower = cmd_name.to_lowercase();
            if cmd_lower == filter_lower {
                exact.push(cmd);
            } else if cmd_lower.starts_with(&filter_lower) {
                prefix.push(cmd);
            } else if is_subsequence(&filter_lower, &cmd_lower) {
                fuzzy.push(cmd);
            }
        }

        exact.extend(prefix);
        exact.extend(fuzzy);
        exact
    }

    pub fn move_up(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = count - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1) % count;
    }

    pub fn selected_command(&self) -> Option<&SlashCommandDef> {
        let items = self.filtered();
        items.get(self.selected).copied()
    }

    /// Look up a command by exact name (e.g. `"/standup"`). Used by the
    /// submit path to detect custom commands that need template expansion.
    pub fn lookup(&self, name: &str) -> Option<&SlashCommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.selected = 0;
        self.filter.clear();
    }

    pub fn render(&self, frame: &mut Frame, composer_area: Rect) {
        if !self.visible {
            return;
        }

        let items = self.filtered();
        if items.is_empty() {
            return;
        }

        let available_above = composer_area.y;
        // Need at least 3 rows: top border + 1 item + bottom border
        if available_above < 3 {
            return;
        }

        let item_count = items.len() as u16;
        let popup_height = (item_count + 2).min(available_above); // +2 for border, clamp to space
        let max_visible = (popup_height - 2) as usize; // items that fit inside borders
        let popup_width = composer_area.width.min(50);

        let popup_y = composer_area.y - popup_height;
        let popup_area = Rect::new(composer_area.x, popup_y, popup_width, popup_height);

        frame.render_widget(Clear, popup_area);

        // Scroll the visible window to keep the selected item in view
        let scroll_offset = if self.selected >= max_visible {
            self.selected - max_visible + 1
        } else {
            0
        };

        let filter_lower = self.filter.to_lowercase();
        let lines: Vec<Line<'_>> = items
            .iter()
            .skip(scroll_offset)
            .take(max_visible)
            .enumerate()
            .map(|(i, cmd)| {
                let actual_index = i + scroll_offset;
                let base = if actual_index == self.selected {
                    theme::popup_selected()
                } else {
                    theme::dim()
                };
                let highlight = base.add_modifier(
                    ratatui::style::Modifier::UNDERLINED | ratatui::style::Modifier::BOLD,
                );
                // Pad command to width 12 (leading space + name), then append description.
                let mut spans: Vec<Span<'_>> = Vec::new();
                spans.push(Span::styled(" ", base));
                let matched = subsequence_match_positions(&filter_lower, &cmd.name);
                let name_chars: Vec<char> = cmd.name.chars().collect();
                for (idx, ch) in name_chars.iter().enumerate() {
                    let style = if matched.contains(&idx) {
                        highlight
                    } else {
                        base
                    };
                    spans.push(Span::styled(ch.to_string(), style));
                }
                let pad = 12usize.saturating_sub(name_chars.len());
                spans.push(Span::styled(" ".repeat(pad + 1), base));
                spans.push(Span::styled(cmd.description.clone(), base));
                if cmd.template.is_some() {
                    // Mark user-defined commands so they can't be confused with built-ins.
                    spans.push(Span::styled("  (user)", theme::dim()));
                }
                Line::from(spans)
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(" Commands ");

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, popup_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_returns_all() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/");
        assert!(popup.is_visible());
        assert_eq!(popup.filtered().len(), BUILTIN_COMMANDS.len());
    }

    #[test]
    fn prefix_filtering_ranks_prefix_before_fuzzy() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/he");
        let items = popup.filtered();
        assert!(!items.is_empty());
        // The top result must be a prefix match (/help, /history), not fuzzy.
        let first = &items[0].name;
        assert!(
            first.starts_with("/he"),
            "top match for /he should be a prefix match, got {first}"
        );
    }

    #[test]
    fn exact_match_sorts_first() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/help");
        let items = popup.filtered();
        assert!(!items.is_empty());
        assert_eq!(items[0].name, "/help");
    }

    #[test]
    fn no_match_returns_empty() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/zzzzz");
        assert!(popup.filtered().is_empty());
    }

    #[test]
    fn up_down_wrapping() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/");
        let count = popup.filtered().len();

        assert_eq!(popup.selected, 0);
        popup.move_up();
        assert_eq!(popup.selected, count - 1);
        popup.move_down();
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/");
        assert!(popup.is_visible());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn space_in_text_hides_popup() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/settings foo");
        assert!(!popup.is_visible());
    }

    #[test]
    fn schedule_command_in_list() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/schedule");
        let items = popup.filtered();
        assert!(
            items.iter().any(|c| c.name == "/schedule"),
            "/schedule should appear in filtered commands"
        );
    }

    #[test]
    fn all_known_commands_present() {
        let names: Vec<&str> = BUILTIN_COMMANDS.iter().map(|(n, _)| *n).collect();
        let expected = [
            "/help",
            "/settings",
            "/usage",
            "/plan",
            "/mode",
            "/compact",
            "/clear",
            "/undo",
            "/memory",
            "/history",
            "/logs",
            "/doctor",
            "/heal",
            "/stats",
            "/xp",
            "/evolution",
            "/card",
            "/pairing",
            "/update",
            "/sessions",
            "/save",
            "/new",
            "/fork",
            "/plugins",
            "/projects",
            "/schedule",
            "/migrate",
            "/restart",
        ];
        assert_eq!(
            BUILTIN_COMMANDS.len(),
            expected.len(),
            "BUILTIN_COMMANDS count mismatch"
        );
        for cmd in &expected {
            assert!(names.contains(cmd), "missing command: {cmd}");
        }
    }

    #[test]
    fn filter_st_ranks_prefix_before_fuzzy() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/st");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        // /stats has the prefix; /settings is a fuzzy subsequence (s…t) —
        // prefix matches must rank ahead of fuzzy matches.
        let stats_pos = names.iter().position(|n| *n == "/stats").unwrap();
        let settings_pos = names.iter().position(|n| *n == "/settings");
        assert!(settings_pos.is_some(), "fuzzy should include /settings");
        assert!(
            stats_pos < settings_pos.unwrap(),
            "prefix /stats must outrank fuzzy /settings, got names: {names:?}"
        );
    }

    #[test]
    fn filter_lo_matches_logs() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/lo");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"/logs"), "should match /logs");
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/stg");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"/settings"),
            "subsequence 'stg' should match /settings, got {names:?}"
        );
    }

    #[test]
    fn fuzzy_is_case_insensitive() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/STG");
        let items = popup.filtered();
        assert!(items.iter().any(|c| c.name == "/settings"));
    }

    #[test]
    fn fuzzy_ranks_exact_above_prefix_above_fuzzy() {
        let mut popup = CommandPopup::new();
        // "hlp" matches /help as subsequence, /plugins as subsequence, and has no exact or prefix.
        popup.update_filter("/hlp");
        let items = popup.filtered();
        assert!(
            items.iter().any(|c| c.name == "/help"),
            "fuzzy 'hlp' should match /help"
        );
    }

    #[test]
    fn is_subsequence_basic() {
        assert!(is_subsequence("", "anything"));
        assert!(is_subsequence("abc", "aabbcc"));
        assert!(is_subsequence("stg", "settings"));
        assert!(!is_subsequence("xyz", "settings"));
        assert!(!is_subsequence("ts", "st"));
    }

    #[test]
    fn nonsense_filter_still_empty() {
        let mut popup = CommandPopup::new();
        // Pick a string with no possible subsequence match anywhere.
        popup.update_filter("/zqxj");
        assert!(
            popup.filtered().is_empty(),
            "expected empty results for /zqxj"
        );
    }

    // ── Custom command integration ──

    fn custom(name: &str, desc: &str, body: &str) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            description: desc.to_string(),
            template: body.to_string(),
        }
    }

    #[test]
    fn with_custom_appends_user_commands() {
        let popup = CommandPopup::with_custom(vec![custom("/standup", "Daily", "body")]);
        let names: Vec<&str> = popup.commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"/standup"));
        // Built-ins must still be present and come first.
        assert_eq!(names[0], BUILTIN_COMMANDS[0].0);
    }

    #[test]
    fn custom_command_appears_in_filter_results() {
        let popup = CommandPopup::with_custom(vec![custom("/standup", "Daily", "body")]);
        let mut popup = popup;
        popup.update_filter("/sta");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"/standup"), "got {names:?}");
    }

    #[test]
    fn name_collision_with_builtin_drops_custom() {
        let popup = CommandPopup::with_custom(vec![custom("/help", "shadowed", "x")]);
        let help_descs: Vec<&str> = popup
            .commands
            .iter()
            .filter(|c| c.name == "/help")
            .map(|c| c.description.as_str())
            .collect();
        assert_eq!(help_descs, vec!["Show available commands"]);
    }

    #[test]
    fn lookup_finds_builtin_and_custom() {
        let popup = CommandPopup::with_custom(vec![custom("/standup", "Daily", "body")]);
        assert!(popup.lookup("/help").is_some());
        let st = popup.lookup("/standup").unwrap();
        assert_eq!(st.template.as_deref(), Some("body"));
        assert!(popup.lookup("/notathing").is_none());
    }

    #[test]
    fn builtin_lookup_has_no_template() {
        let popup = CommandPopup::new();
        let help = popup.lookup("/help").unwrap();
        assert!(help.template.is_none(), "built-ins must not have templates");
    }

    #[test]
    fn builtins_rank_above_custom_in_prefix_tier() {
        // Both `/setting-x` (custom) and `/settings` (builtin) prefix-match `/set`.
        // Built-ins are stored first, so within the prefix tier they should win.
        let popup = CommandPopup::with_custom(vec![custom("/setting-x", "user", "body")]);
        let mut popup = popup;
        popup.update_filter("/set");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name.as_str()).collect();
        let builtin_pos = names.iter().position(|n| *n == "/settings").unwrap();
        let custom_pos = names.iter().position(|n| *n == "/setting-x").unwrap();
        assert!(
            builtin_pos < custom_pos,
            "built-in /settings must rank above custom /setting-x; got {names:?}"
        );
    }
}
