use super::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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
    (
        "/heartbeat",
        "Recurring prompt that re-enters this session when idle (alias: /hb)",
    ),
    (
        "/worktree",
        "Isolated git checkouts to code in (alias: /wt)",
    ),
    ("/mode", "Switch collaboration mode"),
    ("/think", "Show or set the thinking level"),
    ("/model", "Switch LLM provider/model"),
    ("/theme", "Pick TUI theme (alias: /skin)"),
    // Conversation
    (
        "/context",
        "Context-window usage breakdown (system prompt, tools, history)",
    ),
    ("/compact", "Compact conversation history"),
    ("/clear", "Clear conversation"),
    ("/cancel", "Stop the current in-progress turn"),
    ("/undo", "Undo last agent turn"),
    ("/resume-turn", "Continue a reply that was cut short"),
    ("/btw", "Ask an ephemeral side question (not persisted)"),
    // Context
    ("/memory", "Show memory context"),
    ("/pwd", "Show this session's working directory"),
    ("/history", "Show conversation history"),
    ("/logs", "Show activity log"),
    // Diagnostics
    ("/doctor", "Run diagnostics"),
    ("/dream", "Run memory consolidation now"),
    ("/heal", "Run self-healing maintenance sweep"),
    (
        "/reload",
        "Hot-reload MCP servers and/or user skills (mcp|skills)",
    ),
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
    (
        "/export",
        "Export current session (json|csv|txt|md) to a file or the clipboard",
    ),
    // Integrations
    ("/plugins", "Manage plugins and channels"),
    ("/skills", "Manage skills (alias of /plugins)"),
    ("/schedule", "Manage schedules"),
    ("/migrate", "Import from another agent"),
    (
        "/browser",
        "Manage browser CDP connection (connect|disconnect|status)",
    ),
    ("/mcp", "Show configured MCP servers"),
    ("/agents", "View and control sub-agents"),
    (
        "/handoff",
        "Continue this conversation on a messaging channel",
    ),
    (
        "/terminal-setup",
        "Install IDE terminal keybindings (auto|vscode|cursor|windsurf)",
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

/// Greedy case-insensitive subsequence match of `needle` against
/// `haystack`. Returns the char indices of `haystack` that matched, or
/// `None` when `needle` is not a subsequence.
///
/// Both sides are lowercased with the full `to_lowercase` expansion, and
/// matching runs over the flattened haystack stream. One haystack char can
/// expand to several (`'İ'` → `"i\u{307}"`), so each flattened char keeps
/// a map back to its original index — a needle char that hits the second
/// char of an expansion still highlights the right original char.
fn subsequence_match(needle: &str, haystack: &str) -> Option<Vec<usize>> {
    let mut needle = needle.chars().flat_map(char::to_lowercase).peekable();
    let flat = haystack
        .chars()
        .enumerate()
        .flat_map(|(orig, c)| c.to_lowercase().map(move |lc| (orig, lc)));
    let mut matched: Vec<usize> = Vec::new();
    for (orig, lc) in flat {
        let Some(&want) = needle.peek() else { break };
        if lc == want {
            needle.next();
            if matched.last() != Some(&orig) {
                matched.push(orig);
            }
        }
    }
    needle.peek().is_none().then_some(matched)
}

/// Char indices in `haystack` to underline for `needle` in the command
/// popup. Empty needle or no match returns an empty set.
fn subsequence_match_positions(needle: &str, haystack: &str) -> std::collections::HashSet<usize> {
    subsequence_match(needle, haystack)
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// True when every char of `needle` appears in `haystack` in order (not
/// necessarily contiguously), case-insensitively.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    subsequence_match(needle, haystack).is_some()
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
        // Show the popup only while the composer holds a single unterminated
        // `/token`. Any whitespace ends it — a tab or newline finishes the token
        // just as a space does, and matching only on `' '` left the popup open
        // filtering on a query containing whitespace.
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
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
        // Borderless menu surface (tinted bg, inset padding).
        let inner = popup_utils::render_menu_surface(frame, popup_area);
        let max_visible = inner.height as usize; // items that fit inside the surface

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

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/help` left out `/goal`, `/heartbeat`, `/context` and more, and the
    /// popup left out `/think`, `/reload` and `/mcp`: a command was only
    /// discoverable from whichever list someone remembered to edit.
    #[test]
    fn help_and_popup_list_the_same_commands() {
        let help = crate::tui::commands::HELP_TEXT;
        let in_help: Vec<&str> = help
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|w| w.starts_with('/'))
            .collect();
        for (name, _) in BUILTIN_COMMANDS {
            assert!(
                in_help.contains(name),
                "{name} is in the popup but not in /help"
            );
        }
        // `/uninstall` deletes everything: listed in help, never one keypress away.
        for name in in_help.iter().filter(|n| **n != "/uninstall") {
            assert!(
                BUILTIN_COMMANDS.iter().any(|(n, _)| n == name),
                "{name} is in /help but not in the popup"
            );
        }
    }

    #[test]
    fn empty_filter_returns_all() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/");
        assert!(popup.is_visible());
        assert_eq!(popup.filtered().len(), BUILTIN_COMMANDS.len());
    }

    /// Any whitespace terminates the slash token. A tab or newline finishes
    /// `/help` just as a space does; leaving the popup open would filter on a
    /// query that can never match a command name.
    #[test]
    fn whitespace_after_slash_token_hides_popup() {
        for text in [
            "/help ",
            "/help\tmore",
            "/help\nmore",
            "/ help",
            "not /help",
        ] {
            let mut popup = CommandPopup::new();
            popup.update_filter(text);
            assert!(
                !popup.is_visible(),
                "{text:?} should not keep the command popup open"
            );
        }
        // An unterminated token still shows the popup.
        let mut popup = CommandPopup::new();
        popup.update_filter("/hel");
        assert!(popup.is_visible(), "/hel is still in progress");
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
        // Case-insensitive on both sides, including multi-char expansions.
        assert!(is_subsequence("STG", "Settings"));
        assert!(is_subsequence("\u{307}", "İ"));
        assert!(is_subsequence("İx", "aİbx"));
        assert!(!is_subsequence("İ", "i"));
    }

    #[test]
    fn match_positions_report_original_char_indices() {
        // (needle, haystack, expected original char indices)
        let cases: &[(&str, &str, &[usize])] = &[
            ("stg", "/settings", &[1, 3, 7]),
            ("STG", "/settings", &[1, 3, 7]),
            ("hlp", "/Help", &[1, 3, 4]),
            ("", "/help", &[]),
            ("xyz", "/settings", &[]),
            ("ts", "/st", &[]),
            // 'İ' lowercases to "i\u{307}": a needle char that lands on the
            // second char of the expansion must still map back to 'İ'.
            ("\u{307}", "/İ", &[1]),
            ("i\u{307}x", "/İx", &[1, 2]),
            ("İx", "/aİbx", &[2, 4]),
            // Both expansion chars come from one original char: one index.
            ("İ", "/İ", &[1]),
        ];
        for (needle, haystack, expected) in cases {
            let mut got: Vec<usize> = subsequence_match_positions(needle, haystack)
                .into_iter()
                .collect();
            got.sort_unstable();
            assert_eq!(&got[..], *expected, "needle {needle:?} in {haystack:?}");
        }
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
        // Built-in resolves to the matching command (a built-in has no template).
        let help = popup.lookup("/help").expect("/help is a built-in");
        assert_eq!(help.name, "/help");
        assert!(help.template.is_none());
        // Custom resolves to its own template body.
        let st = popup.lookup("/standup").unwrap();
        assert_eq!(st.name, "/standup");
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
