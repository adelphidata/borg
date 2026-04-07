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
        description: "Toggle plan mode (review before proceeding)",
    },
    SlashCommandDef {
        name: "/mode",
        description: "Switch mode (default/execute/plan)",
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
        description: "Show activity log (error|warn|info|debug|all|raw)",
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
        description: "Show channel pairing info",
    },
    SlashCommandDef {
        name: "/update",
        description: "Update borg to latest version",
    },
    // Sessions
    SlashCommandDef {
        name: "/sessions",
        description: "Browse saved sessions",
    },
    SlashCommandDef {
        name: "/save",
        description: "Save current session",
    },
    SlashCommandDef {
        name: "/new",
        description: "Start new session",
    },
    SlashCommandDef {
        name: "/load",
        description: "Load a saved session by ID",
    },
    // Integrations
    SlashCommandDef {
        name: "/plugins",
        description: "Manage plugins, channels, and tools",
    },
    SlashCommandDef {
        name: "/schedule",
        description: "Manage scheduled tasks",
    },
    SlashCommandDef {
        name: "/migrate",
        description: "Import from another agent",
    },
    SlashCommandDef {
        name: "/restart",
        description: "Restart gateway server",
    },
];

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

        // Collect matches: exact first, then prefix
        let mut exact = Vec::new();
        let mut prefix = Vec::new();

        for cmd in COMMANDS {
            let cmd_name = &cmd.name[1..]; // strip leading '/'
            if cmd_name == filter_lower {
                exact.push(cmd);
            } else if cmd_name.starts_with(&filter_lower) {
                prefix.push(cmd);
            }
        }

        exact.extend(prefix);
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
    fn prefix_filtering() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/he");
        let items = popup.filtered();
        assert!(items.iter().all(|c| c.name.starts_with("/he")));
        assert!(!items.is_empty());
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
            "/load",
            "/plugins",
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
    fn filter_st_matches_stats_and_schedule() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/st");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name).collect();
        assert!(names.contains(&"/stats"), "should match /stats");
        // /settings starts with /se, not /st — so it should NOT match
        assert!(
            !names.contains(&"/settings"),
            "/settings should not match /st"
        );
    }

    #[test]
    fn filter_lo_matches_load_and_logs() {
        let mut popup = CommandPopup::new();
        popup.update_filter("/lo");
        let items = popup.filtered();
        let names: Vec<&str> = items.iter().map(|c| c.name).collect();
        assert!(names.contains(&"/load"), "should match /load");
        assert!(names.contains(&"/logs"), "should match /logs");
    }
}
