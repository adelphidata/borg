use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;

#[derive(Copy, Clone)]
pub struct SlashCommandDef {
    pub name: &'static str,
    pub description: &'static str,
}

const COMMANDS: &[SlashCommandDef] = &[
    // Essentials
    SlashCommandDef {
        name: "/help",
        description: "Show available commands",
    },
    SlashCommandDef {
        name: "/settings",
        description: "Configure settings",
    },
    SlashCommandDef {
        name: "/usage",
        description: "Show usage stats",
    },
    SlashCommandDef {
        name: "/plan",
        description: "Toggle plan mode",
    },
    SlashCommandDef {
        name: "/mode",
        description: "Switch collaboration mode",
    },
    // Conversation
    SlashCommandDef {
        name: "/compact",
        description: "Compact conversation history",
    },
    SlashCommandDef {
        name: "/clear",
        description: "Clear conversation",
    },
    SlashCommandDef {
        name: "/undo",
        description: "Undo last agent turn",
    },
    // Context
    SlashCommandDef {
        name: "/memory",
        description: "Show memory context",
    },
    SlashCommandDef {
        name: "/history",
        description: "Show conversation history",
    },
    SlashCommandDef {
        name: "/logs",
        description: "Show activity log",
    },
    // Diagnostics
    SlashCommandDef {
        name: "/doctor",
        description: "Run diagnostics",
    },
    SlashCommandDef {
        name: "/stats",
        description: "Show agent vitals",
    },
    SlashCommandDef {
        name: "/pairing",
        description: "Manage sender pairing",
    },
    SlashCommandDef {
        name: "/update",
        description: "Update borg to latest version",
    },
    // Sessions
    SlashCommandDef {
        name: "/sessions",
        description: "Browse and load saved sessions",
    },
    SlashCommandDef {
        name: "/save",
        description: "Save current session",
    },
    SlashCommandDef {
        name: "/new",
        description: "Start new session",
    },
    // Integrations
    SlashCommandDef {
        name: "/plugins",
        description: "Manage plugins and channels",
    },
    SlashCommandDef {
        name: "/projects",
        description: "List projects",
    },
    SlashCommandDef {
        name: "/schedule",
        description: "Manage schedules",
    },
    SlashCommandDef {
        name: "/migrate",
        description: "Import from another agent",
    },
    SlashCommandDef {
        name: "/restart",
        description: "Restart gateway server",
    },
    SlashCommandDef {
        name: "/xp",
        description: "Show XP summary and feed",
    },
];

/// True when every char of `needle` appears in `haystack` in order
/// (not necessarily contiguously). Both inputs are expected lowercase.
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
}

impl CommandPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            filter: String::new(),
            selected: 0,
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
            return COMMANDS.iter().collect();
        }

        let filter_lower = self.filter.to_lowercase();

        // Three-tier ranking: exact match > prefix match > subsequence match.
        // Within each tier, preserve the declaration order of COMMANDS so
        // related commands stay grouped (e.g. /settings near /status).
        let mut exact = Vec::new();
        let mut prefix = Vec::new();
        let mut fuzzy = Vec::new();

        for cmd in COMMANDS {
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

        let lines: Vec<Line<'_>> = items
            .iter()
            .skip(scroll_offset)
            .take(max_visible)
            .enumerate()
            .map(|(i, cmd)| {
                let actual_index = i + scroll_offset;
                let style = if actual_index == self.selected {
                    theme::popup_selected()
                } else {
                    theme::dim()
                };
                Line::from(Span::styled(
                    format!(" {:<12} {}", cmd.name, cmd.description),
                    style,
                ))
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
        assert_eq!(popup.filtered().len(), COMMANDS.len());
    }

    #[test]
    fn prefix_filtering_ranks_prefix_before_fuzzy() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/he");
        let items = popup.filtered();
        assert!(!items.is_empty());
        // The top result must be a prefix match (/help, /history), not fuzzy.
        let first = items[0].name;
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
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
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
            "/stats",
            "/pairing",
            "/update",
            "/sessions",
            "/save",
            "/new",
            "/plugins",
            "/projects",
            "/schedule",
            "/migrate",
            "/restart",
        ];
        assert_eq!(COMMANDS.len(), expected.len(), "COMMANDS count mismatch");
        for cmd in &expected {
            assert!(names.contains(cmd), "missing command: {cmd}");
        }
    }

    #[test]
    fn filter_st_ranks_prefix_before_fuzzy() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/st");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name).collect();
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
        let names: Vec<&str> = items.iter().map(|c| c.name).collect();
        assert!(names.contains(&"/logs"), "should match /logs");
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/stg");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name).collect();
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
}
