//! `/mcp` popup — read-only operator view of configured MCP servers.
//!
//! Mirrors what `borg mcp list` shows on the command line: one row per
//! configured server with its transport, lifecycle state (enabled in
//! config / live-disabled / connected / failed), and live tool count.
//! Read-only by design: enable/disable goes through `borg mcp ...` or
//! `borg settings set mcp.disabled_servers=…` so the trust model stays
//! anchored in the CLI surface.

use super::custom_terminal::Frame;
use anyhow::Result;
use cortex::config::Config;
use cortex::mcp_clients::{server_status_snapshot, McpServerSnapshot};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

pub struct McpPopup {
    visible: bool,
    rows: Vec<McpServerSnapshot>,
    cursor: usize,
}

impl McpPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            rows: Vec::new(),
            cursor: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Load the current snapshot from cortex + open the popup. Cheap; the
    /// snapshot clones a `BTreeMap` from the live registry plus the
    /// already-in-memory `config.mcp.servers` map.
    pub fn show(&mut self, config: &Config) {
        self.visible = true;
        self.cursor = 0;
        self.rows = server_status_snapshot(config);
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        if !self.visible {
            return;
        }
        match key.code {
            KeyCode::Esc => self.dismiss(),
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.rows.is_empty() {
                    self.cursor = if self.cursor == 0 {
                        self.rows.len() - 1
                    } else {
                        self.cursor - 1
                    };
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.cursor = (self.cursor + 1) % self.rows.len();
                }
            }
            _ => {}
        }
    }

    /// Test helper: inspect rendered row text without owning a real
    /// terminal frame. Returns one string per server in render order.
    #[cfg(test)]
    pub fn rendered_rows_for_test(&self) -> Vec<String> {
        self.rows.iter().map(render_row).collect()
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "MCP Servers", 3, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                " No MCP servers configured. Use `borg mcp add …` to register one.".to_string(),
                theme::dim(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(" {} server(s) configured", self.rows.len()),
                theme::dim(),
            )));
            lines.push(Line::default());
            for (i, row) in self.rows.iter().enumerate() {
                let label = format!("  {}", render_row(row));
                let style = if i == self.cursor {
                    theme::popup_selected()
                } else {
                    ratatui::style::Style::default()
                };
                lines.push(Line::from(Span::styled(label, style)));
            }
        }

        let scroll_offset = self.cursor.saturating_sub(content_height.saturating_sub(3));
        let visible_lines: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll_offset)
            .take(content_height)
            .collect();

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible_lines), content_area);

        popup_utils::render_footer(
            frame,
            inner,
            " Esc: close   Use `borg mcp` to add/remove/test servers ",
        );
    }
}

fn render_row(row: &McpServerSnapshot) -> String {
    let state = row.state_label();
    let tools = if row.tool_count == 0 && row.rejected_count == 0 {
        "0 tools".to_string()
    } else if row.rejected_count == 0 {
        format!("{} tools", row.tool_count)
    } else {
        format!("{} tools ({} rejected)", row.tool_count, row.rejected_count)
    };
    format!(
        "{:<20} {:<8} {:<14} {}",
        row.name, row.transport, state, tools
    )
}

impl PopupHandler for McpPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        self.handle_key(key);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::{McpServerConfig, TransportSpec};
    use std::collections::HashMap;

    fn config_with_servers(specs: Vec<(&str, TransportSpec, bool)>) -> Config {
        let mut cfg = Config::default();
        cfg.mcp.enabled = true;
        let mut servers = HashMap::new();
        for (name, transport, enabled) in specs {
            servers.insert(
                name.to_string(),
                McpServerConfig {
                    enabled,
                    transport,
                    connect_timeout_ms: 60_000,
                    call_timeout_ms: 120_000,
                    rate_limit_per_minute: 60,
                    rate_limit_burst: 10,
                    tools: Default::default(),
                },
            );
        }
        cfg.mcp.servers = servers;
        cfg
    }

    fn stdio(cmd: &str) -> TransportSpec {
        TransportSpec::Stdio {
            command: cmd.to_string(),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn fresh_popup_is_hidden_and_has_no_rows() {
        let popup = McpPopup::new();
        assert!(!popup.is_visible());
        assert!(popup.rendered_rows_for_test().is_empty());
    }

    #[test]
    fn show_loads_configured_servers_sorted_by_name() {
        let cfg = config_with_servers(vec![
            ("zeta", stdio("uvx"), true),
            ("alpha", stdio("uvx"), true),
            ("mid", stdio("uvx"), true),
        ]);
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        let rows = popup.rendered_rows_for_test();
        assert_eq!(rows.len(), 3);
        // Sorted alphabetically — operators scanning a long list need
        // deterministic order, not insertion order.
        assert!(rows[0].starts_with("alpha"));
        assert!(rows[1].starts_with("mid"));
        assert!(rows[2].starts_with("zeta"));
    }

    #[test]
    fn config_disabled_server_renders_as_disabled() {
        let cfg = config_with_servers(vec![("off", stdio("uvx"), false)]);
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        let row = popup.rendered_rows_for_test().pop().unwrap();
        assert!(
            row.contains("disabled"),
            "config-disabled server must render as `disabled`, got: {row}"
        );
    }

    #[test]
    fn live_disabled_overrides_enabled_flag() {
        // A server enabled in config but live-listed in
        // `mcp.disabled_servers` is the operator's live kill-switch —
        // the popup MUST surface that, otherwise an operator who just
        // ran `borg settings set mcp.disabled_servers=…` would think
        // the change didn't take.
        let mut cfg = config_with_servers(vec![("muted", stdio("uvx"), true)]);
        cfg.mcp.disabled_servers = vec!["muted".to_string()];
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        let row = popup.rendered_rows_for_test().pop().unwrap();
        assert!(
            row.contains("live-disabled"),
            "live-disabled must take precedence over enabled=true, got: {row}"
        );
    }

    #[test]
    fn transport_kind_is_visible() {
        let cfg = config_with_servers(vec![
            (
                "remote",
                TransportSpec::Http {
                    url: "https://example.com/mcp".to_string(),
                    headers: HashMap::new(),
                    auth: Default::default(),
                    client_cert: None,
                    client_key: None,
                },
                true,
            ),
            ("local", stdio("uvx"), true),
        ]);
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        let rows = popup.rendered_rows_for_test();
        let local_row = rows.iter().find(|r| r.starts_with("local")).unwrap();
        let remote_row = rows.iter().find(|r| r.starts_with("remote")).unwrap();
        assert!(local_row.contains("stdio"), "{local_row}");
        assert!(remote_row.contains("http"), "{remote_row}");
    }

    #[test]
    fn navigation_wraps_with_arrow_keys() {
        let cfg = config_with_servers(vec![("a", stdio("uvx"), true), ("b", stdio("uvx"), true)]);
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        assert_eq!(popup.cursor, 0);
        popup.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(popup.cursor, 1, "Up at top must wrap to last entry");
        popup.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(popup.cursor, 0, "Down at bottom must wrap to first");
    }

    #[test]
    fn esc_dismisses() {
        let cfg = config_with_servers(vec![("a", stdio("uvx"), true)]);
        let mut popup = McpPopup::new();
        popup.show(&cfg);
        assert!(popup.is_visible());
        popup.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!popup.is_visible());
    }

    /// PopupHandler must NOT request text input — the global `/`
    /// dispatcher uses `wants_text_input()` to decide whether to
    /// hijack `/` for slash-command parsing. An /mcp popup that
    /// accidentally returned `true` here would break `/help` typed
    /// while the popup is open.
    #[test]
    fn default_does_not_want_text_input() {
        let popup = McpPopup::new();
        assert!(!PopupHandler::wants_text_input(&popup));
    }
}
