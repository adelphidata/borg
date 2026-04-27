use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use tokio::sync::Mutex;

use cortex::config::Config;

use crate::daemon_client::DaemonClient;

use super::app::{AppAction, PopupHandler};
use super::theme;

/// Number of fixed banner lines (BORG / name / model) at the top of the
/// Overview tab. Used by the resize path to splice in fresh body lines
/// without clobbering the banner.
const OVERVIEW_BANNER_LINES: usize = 3;

/// Tabs available in the status popup. Each variant routes the body of the
/// popup to a distinct content builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTab {
    /// Combined dashboard — banner, evolution, vitals, bond, archetype, history.
    Overview,
    /// `/evolution` overview: stage, XP, archetype momentum, readiness, hints.
    Evolution,
    /// `/xp` feed and aggregates.
    Xp,
    /// Standalone archetype score table.
    ArchetypeScores,
    /// Evolution history timeline.
    History,
}

impl StatusTab {
    /// Ordered tab list used for rendering and Left/Right cycling.
    pub const ALL: [StatusTab; 5] = [
        StatusTab::Overview,
        StatusTab::Evolution,
        StatusTab::Xp,
        StatusTab::ArchetypeScores,
        StatusTab::History,
    ];

    /// Short label shown in the tab bar.
    fn label(self) -> &'static str {
        match self {
            StatusTab::Overview => "Overview",
            StatusTab::Evolution => "Evolution",
            StatusTab::Xp => "XP",
            StatusTab::ArchetypeScores => "Archetypes",
            StatusTab::History => "History",
        }
    }

    /// Advance one step in `ALL`, wrapping at the end.
    fn next(self) -> StatusTab {
        let idx = StatusTab::ALL.iter().position(|&t| t == self).unwrap_or(0);
        StatusTab::ALL[(idx + 1) % StatusTab::ALL.len()]
    }

    /// Retreat one step in `ALL`, wrapping at the start.
    fn prev(self) -> StatusTab {
        let idx = StatusTab::ALL.iter().position(|&t| t == self).unwrap_or(0);
        let n = StatusTab::ALL.len();
        StatusTab::ALL[(idx + n - 1) % n]
    }
}

pub struct StatusPopup {
    visible: bool,
    scroll_offset: usize,
    lines: Vec<Line<'static>>,
    /// Last rendered inner width (for detecting resize). 0 means "not yet
    /// rendered"; on first render the Overview tab refetches at the real
    /// width to size the evolution card correctly.
    last_width: u16,
    /// Which tab is currently displayed.
    current_tab: StatusTab,
    /// Daemon client for status RPCs. `None` only in unit tests where the
    /// popup is poked at via direct field access (no `show()` call).
    client: Option<Arc<Mutex<DaemonClient>>>,
}

impl StatusPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            scroll_offset: 0,
            lines: Vec::new(),
            last_width: 0,
            current_tab: StatusTab::Overview,
            client: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Read the currently-selected tab. Primarily for tests.
    #[allow(dead_code)]
    pub fn current_tab(&self) -> StatusTab {
        self.current_tab
    }

    /// Open the popup on the default `Overview` tab.
    pub fn show(&mut self, config: &Config, client: &Arc<Mutex<DaemonClient>>) {
        self.current_tab = StatusTab::Overview;
        self.client = Some(client.clone());
        self.reload(config);
    }

    /// Open the popup on a specific tab.
    pub fn show_tab(&mut self, config: &Config, tab: StatusTab, client: &Arc<Mutex<DaemonClient>>) {
        self.current_tab = tab;
        self.client = Some(client.clone());
        self.reload(config);
    }

    /// Rebuild the popup content for the current tab via daemon RPCs.
    fn reload(&mut self, config: &Config) {
        self.visible = true;
        self.scroll_offset = 0;
        self.lines.clear();
        self.last_width = 0;

        match self.current_tab {
            StatusTab::Overview => self.build_overview(config, 0),
            StatusTab::Evolution => self.build_evolution(),
            StatusTab::Xp => self.build_xp(),
            StatusTab::ArchetypeScores => self.build_archetype_scores(config),
            StatusTab::History => self.build_history(config),
        }
    }

    /// Run a daemon RPC from the sync popup-handler context. Mirrors the
    /// pattern used by `projects_popup` etc.
    fn block_on_rpc<F, T>(&self, f: F) -> Option<T>
    where
        F: for<'b> FnOnce(
            &'b mut DaemonClient,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T>> + Send + 'b>,
        >,
    {
        let client = self.client.clone()?;
        // `try_current` so unit tests that build an `App` outside the daemon
        // event loop (no active tokio runtime in scope) don't panic; they
        // simply skip the RPC and the popup falls back to its
        // "Status unavailable." path.
        let handle = tokio::runtime::Handle::try_current().ok()?;
        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                let mut c = client.lock().await;
                f(&mut c).await.ok()
            })
        })
    }

    fn build_overview(&mut self, config: &Config, card_width: u32) {
        let version = env!("CARGO_PKG_VERSION");
        let name = config.user.agent_name.as_deref().unwrap_or("Borg");

        // Banner
        self.lines.push(Line::from(vec![
            Span::styled(
                "BORG",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::from(" "),
            Span::styled(format!("v{version}"), theme::dim()),
        ]));
        self.lines.push(Line::from(vec![
            Span::styled("name:  ", theme::dim()),
            Span::from(name.to_string()),
        ]));
        self.lines.push(Line::from(vec![
            Span::styled("model: ", theme::dim()),
            Span::from(config.llm.model.clone()),
        ]));

        let snap = self.block_on_rpc(move |c| Box::pin(c.get_status_overview_text(card_width)));
        let snap = match snap {
            Some(s) => s,
            None => {
                tracing::warn!("status_popup: overview RPC failed");
                self.push_section("Status unavailable.");
                return;
            }
        };

        if !snap.compact_class_line.is_empty() {
            self.lines.push(Line::from(vec![
                Span::styled("class: ", theme::dim()),
                Span::styled(snap.compact_class_line, Style::default().fg(theme::CYAN)),
            ]));
            self.lines.push(Line::default());
        }
        self.push_section(&snap.text);
    }

    fn build_evolution(&mut self) {
        match self.block_on_rpc(|c| Box::pin(c.get_evolution_text())) {
            Some(snap) => self.push_section(&snap.text),
            None => {
                tracing::warn!("status_popup: /evolution RPC failed");
                self.push_section("Evolution unavailable.");
            }
        }
    }

    fn build_xp(&mut self) {
        match self.block_on_rpc(|c| Box::pin(c.get_xp_text())) {
            Some(snap) => self.push_section(&snap.text),
            None => {
                tracing::warn!("status_popup: /xp RPC failed");
                self.push_section("XP unavailable.");
            }
        }
    }

    fn build_archetype_scores(&mut self, config: &Config) {
        if !config.evolution.enabled {
            self.push_section("Evolution system is disabled.");
            return;
        }
        match self.block_on_rpc(|c| Box::pin(c.get_archetype_scores_text())) {
            Some(snap) => self.push_section(&snap.text),
            None => {
                tracing::warn!("status_popup: archetype-scores RPC failed");
                self.push_section("Archetype scores unavailable.");
            }
        }
    }

    fn build_history(&mut self, config: &Config) {
        if !config.evolution.enabled {
            self.push_section("Evolution system is disabled.");
            return;
        }
        match self.block_on_rpc(|c| Box::pin(c.get_evolution_history_text())) {
            Some(snap) => self.push_section(&snap.text),
            None => {
                tracing::warn!("status_popup: history RPC failed");
                self.push_section("History unavailable.");
            }
        }
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<StatusTab> {
        if !self.visible {
            return None;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.dismiss();
                None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.current_tab = self.current_tab.prev();
                Some(self.current_tab)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.current_tab = self.current_tab.next();
                Some(self.current_tab)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                None
            }
            _ => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        if !self.visible {
            return;
        }

        let popup_area = frame.area();

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(" Status ");

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        if inner.height < 4 || inner.width < 10 {
            return;
        }

        // Tab bar along the top row of the inner area.
        let tab_bar = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(self.render_tab_bar(), tab_bar);

        // Content area below the tab bar.
        let content = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(1),
        );

        // Refetch the Overview tab body at the real width on first render and
        // any subsequent resize. The banner (BORG / name / model) stays
        // intact; only the body lines (class + sections) are replaced.
        if self.current_tab == StatusTab::Overview && content.width != self.last_width {
            self.last_width = content.width;
            let card_width = (content.width as u32).saturating_sub(4);
            if let Some(snap) =
                self.block_on_rpc(move |c| Box::pin(c.get_status_overview_text(card_width)))
            {
                // Preserve the first 3 banner lines built in `build_overview`.
                self.lines.truncate(OVERVIEW_BANNER_LINES);
                self.scroll_offset = 0;
                if !snap.compact_class_line.is_empty() {
                    self.lines.push(Line::from(vec![
                        Span::styled("class: ", theme::dim()),
                        Span::styled(snap.compact_class_line, Style::default().fg(theme::CYAN)),
                    ]));
                    self.lines.push(Line::default());
                }
                for line in snap.text.lines() {
                    self.lines.push(Line::from(line.to_string()));
                }
                self.lines.push(Line::default());
            }
        }

        let max_scroll = self.lines.len().saturating_sub(content.height as usize);
        let offset = self.scroll_offset.min(max_scroll);

        let paragraph = Paragraph::new(self.lines.clone()).scroll((offset as u16, 0));

        frame.render_widget(paragraph, content);

        // Footer hint
        let footer_y = popup_area.y + popup_area.height.saturating_sub(1);
        if popup_area.width > 40 {
            let hint = " Esc: close  \u{2190}\u{2192}: tabs  \u{2191}\u{2193}: scroll ";
            let hint_x = popup_area.x + 2;
            let hint_area = Rect::new(hint_x, footer_y, hint.len() as u16, 1);
            let hint_widget = Paragraph::new(Line::from(Span::styled(hint, theme::dim())));
            frame.render_widget(hint_widget, hint_area);
        }
    }

    /// Render the single-line tab bar with the active tab highlighted.
    fn render_tab_bar(&self) -> Paragraph<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("[ ", theme::dim()));
        for (i, tab) in StatusTab::ALL.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" | ", theme::dim()));
            }
            let style = if *tab == self.current_tab {
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme::dim()
            };
            spans.push(Span::styled(tab.label().to_string(), style));
        }
        spans.push(Span::styled(" ]", theme::dim()));
        Paragraph::new(Line::from(spans))
    }

    fn push_section(&mut self, text: &str) {
        for line in text.lines() {
            self.lines.push(Line::from(line.to_string()));
        }
        self.lines.push(Line::default());
    }
}

impl PopupHandler for StatusPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        if let Some(_new_tab) = self.handle_key(key) {
            // Rebuild content for the newly-selected tab. `client` was set on
            // `show()`; if the popup got here without a client, `reload` will
            // log a warning and surface "Status unavailable." rather than
            // panic.
            self.reload(config);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_popup_not_visible() {
        let popup = StatusPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn default_tab_is_overview() {
        let popup = StatusPopup::new();
        assert_eq!(popup.current_tab(), StatusTab::Overview);
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        assert!(popup.is_visible());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        let esc = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(esc);
        assert!(!popup.is_visible());
    }

    #[test]
    fn q_dismisses() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        let q = KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::NONE);
        popup.handle_key(q);
        assert!(!popup.is_visible());
    }

    #[test]
    fn scroll_down_increases_offset() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        assert_eq!(popup.scroll_offset, 0);
        let down = KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(down);
        assert_eq!(popup.scroll_offset, 1);
    }

    #[test]
    fn scroll_up_decreases_offset() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        popup.scroll_offset = 5;
        let up = KeyEvent::new(KeyCode::Up, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(up);
        assert_eq!(popup.scroll_offset, 4);
    }

    #[test]
    fn scroll_up_does_not_underflow() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        popup.scroll_offset = 0;
        let up = KeyEvent::new(KeyCode::Up, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(up);
        assert_eq!(popup.scroll_offset, 0);
    }

    #[test]
    fn page_down_scrolls_by_ten() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        let pgdn = KeyEvent::new(KeyCode::PageDown, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(pgdn);
        assert_eq!(popup.scroll_offset, 10);
    }

    #[test]
    fn page_up_scrolls_by_ten() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        popup.scroll_offset = 15;
        let pgup = KeyEvent::new(KeyCode::PageUp, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(pgup);
        assert_eq!(popup.scroll_offset, 5);
    }

    #[test]
    fn handle_key_noop_when_not_visible() {
        let mut popup = StatusPopup::new();
        let down = KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(down);
        assert_eq!(popup.scroll_offset, 0);
        assert!(!popup.is_visible());
    }

    #[test]
    fn right_arrow_cycles_tabs_forward() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        assert_eq!(popup.current_tab(), StatusTab::Overview);
        let right = KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::NONE);
        let change = popup.handle_key(right);
        assert_eq!(change, Some(StatusTab::Evolution));
        assert_eq!(popup.current_tab(), StatusTab::Evolution);
    }

    #[test]
    fn left_arrow_cycles_tabs_backward_with_wrap() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        let left = KeyEvent::new(KeyCode::Left, crossterm::event::KeyModifiers::NONE);
        let change = popup.handle_key(left);
        // From Overview going left wraps to last tab (History).
        assert_eq!(change, Some(StatusTab::History));
        assert_eq!(popup.current_tab(), StatusTab::History);
    }

    #[test]
    fn right_arrow_wraps_at_end() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        popup.current_tab = StatusTab::History;
        let right = KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::NONE);
        popup.handle_key(right);
        assert_eq!(popup.current_tab(), StatusTab::Overview);
    }

    #[test]
    fn tab_next_prev_consistent() {
        for tab in StatusTab::ALL {
            assert_eq!(tab.next().prev(), tab);
            assert_eq!(tab.prev().next(), tab);
        }
    }
}
