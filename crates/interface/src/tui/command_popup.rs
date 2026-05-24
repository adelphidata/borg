use super::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::custom_commands::CustomCommand;
use super::popup_utils;
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
    (
        "/usage",
        "Token spend, model + tool usage over a window (default 30 days)",
    ),
    ("/plan", "Toggle plan mode"),
    (
        "/goal",
        "Set, view, pause, resume, edit, or clear the session goal",
    ),
    ("/mode", "Switch collaboration mode"),
    ("/model", "Switch LLM provider/model"),
    ("/theme", "Pick TUI theme (alias: /skin)"),
    // Conversation
    ("/compact", "Compact conversation history"),
    ("/clear", "Clear conversation"),
    ("/cancel", "Stop the current in-progress turn"),
    ("/undo", "Undo last agent turn"),
    ("/btw", "Ask an ephemeral side question (not persisted)"),
    // Context
    ("/memory", "Show memory context"),
    ("/history", "Show conversation history"),
    ("/logs", "Show activity log"),
    // Diagnostics
    ("/doctor", "Run diagnostics"),
    ("/dream", "Run memory consolidation now"),
    ("/heal", "Run self-healing maintenance sweep"),
    (
        "/poke",
        "Trigger an immediate heartbeat (bypasses quiet hours)",
    ),
    ("/status", "Show agent vitals (alias of /stats)"),
    ("/stats", "Show agent vitals"),
    ("/voice", "Voice mode (on|off|rec|tts|status)"),
    ("/evolution", "Show evolution overview"),
    ("/card", "Print an ASCII share card"),
    ("/pairing", "Manage sender pairing"),
    (
        "/update",
        "Update borg (--prerelease for experimental, --check to dry-run)",
    ),
    // Sessions
    ("/sessions", "Browse and load saved sessions"),
    (
        "/subprocesses",
        "View tracked subprocesses (p: toggle pin, r: reload)",
    ),
    ("/save", "Save current session"),
    ("/new", "Start new session"),
    ("/fork", "Branch the current session into a new one"),
    (
        "/resume",
        "Resume a saved session by id (use /sessions to browse)",
    ),
    ("/export", "Export current session (json|csv|txt)"),
    // Integrations
    ("/plugins", "Manage plugins and channels"),
    ("/skills", "Manage skills (alias of /plugins)"),
    ("/schedule", "Manage schedules"),
    ("/migrate", "Import from another agent"),
    (
        "/browser",
        "Manage browser CDP connection (connect|disconnect|status)",
    ),
    ("/restart", "Restart gateway server"),
    ("/xp", "Show XP summary and feed"),
    ("/exit", "Exit Borg chat"),
];

/// Maximum number of slash commands visible in the popup at one time.
/// Keeps the popup short so users see what's available without it covering
/// the composer; the selected item still scrolls into view.
pub const MAX_VISIBLE_ITEMS: u16 = 4;

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
        // /help and /settings pinned to the top (most-reached-for entries),
        // then the rest A→Z. Within filter tiers (exact/prefix/fuzzy) this
        // ordering carries through, so the popup stays predictable.
        fn rank(name: &str) -> u8 {
            match name {
                "/help" => 0,
                "/settings" => 1,
                _ => 2,
            }
        }
        commands.sort_by(|a, b| {
            rank(&a.name)
                .cmp(&rank(&b.name))
                .then_with(|| a.name.cmp(&b.name))
        });
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

        // Cap visible rows so users always see a small, predictable slate of
        // commands rather than a wall that pushes the composer offscreen.
        // Selected item still scrolls into view via `scroll_offset` below.
        let item_count = items.len() as u16;
        let visible_rows = item_count.min(MAX_VISIBLE_ITEMS);
        let popup_width = composer_area.width.min(50);
        // Need at least 3 rows: top border + 1 item + bottom border.
        let Some(popup_area) = popup_utils::popup_rect_above(
            frame.area(),
            composer_area,
            visible_rows + 2,
            3,
            popup_width,
        ) else {
            return;
        };
        let popup_height = popup_area.height;
        let max_visible = (popup_height - 2) as usize; // items that fit inside borders

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
        // /standup sorts alphabetically into the merged list — confirm it's
        // placed correctly rather than appended at the end.
        let standup_pos = names.iter().position(|n| *n == "/standup").unwrap();
        if let Some(prev) = names.get(standup_pos.wrapping_sub(1)) {
            assert!(
                *prev < "/standup",
                "list not sorted: {prev} before /standup"
            );
        }
        if let Some(next) = names.get(standup_pos + 1) {
            assert!(
                *next > "/standup",
                "list not sorted: /standup before {next}"
            );
        }
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
    fn unfiltered_pins_help_then_settings_then_alphabetical() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name.as_str()).collect();

        assert_eq!(names[0], "/help");
        assert_eq!(names[1], "/settings");

        // Tail (everything after the two pinned entries) must be A→Z.
        let tail: Vec<&str> = names[2..].to_vec();
        let mut sorted_tail = tail.clone();
        sorted_tail.sort();
        assert_eq!(tail, sorted_tail, "tail not alphabetical: {tail:?}");
    }

    #[test]
    fn pinned_settings_outranks_alphabetically_earlier_custom() {
        // /setting-x and /settings both prefix-match /set. /settings is pinned
        // to the top of the global ordering, so even though /setting-x sorts
        // earlier alphabetically, /settings should still come first.
        let mut popup = CommandPopup::with_custom(vec![custom("/setting-x", "user", "body")]);
        popup.update_filter("/set");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name.as_str()).collect();
        let settings_pos = names.iter().position(|n| *n == "/settings").unwrap();
        let setting_x_pos = names.iter().position(|n| *n == "/setting-x").unwrap();
        assert!(
            settings_pos < setting_x_pos,
            "pinned /settings must precede /setting-x; got {names:?}"
        );
    }

    #[test]
    fn filter_he_returns_help_first() {
        // Both /help and /heal prefix-match /he. Pinning makes /help win even
        // though /heal sorts earlier alphabetically.
        let mut popup = CommandPopup::new();
        popup.update_filter("/he");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[0], "/help", "got {names:?}");
    }
}
