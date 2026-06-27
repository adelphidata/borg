use std::sync::Arc;

use super::custom_terminal::Frame;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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
                    .fg(theme::cyan())
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
                Span::styled(snap.compact_class_line, Style::default().fg(theme::cyan())),
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

        let inner = super::popup_utils::render_popup_frame(frame, popup_area, "Status");

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
                        Span::styled(snap.compact_class_line, Style::default().fg(theme::cyan())),
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
                    .fg(theme::cyan())
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

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// Both Esc and 'q' close the popup — keyboard symmetry users expect
    /// from a TUI panel.
    #[test]
    fn esc_and_q_both_dismiss() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut popup = StatusPopup::new();
            popup.visible = true;
            popup.handle_key(k(code));
            assert!(!popup.is_visible(), "key {code:?} should dismiss");
        }
    }

    /// Vertical scrolling: Down/Up move one line, PageDown/PageUp move 10,
    /// and Up at offset 0 must not underflow (regression would saturate the
    /// `u16` offset to 65535 and the content would never paint).
    #[test]
    fn vertical_scroll_offset_steps_and_clamps() {
        let mut popup = StatusPopup::new();
        popup.visible = true;
        assert_eq!(popup.scroll_offset, 0);

        popup.handle_key(k(KeyCode::Down));
        assert_eq!(popup.scroll_offset, 1, "Down should move by one");

        popup.handle_key(k(KeyCode::Up));
        assert_eq!(popup.scroll_offset, 0, "Up should move by one");

        // Underflow guard.
        popup.handle_key(k(KeyCode::Up));
        assert_eq!(popup.scroll_offset, 0, "Up at 0 must clamp");

        popup.handle_key(k(KeyCode::PageDown));
        assert_eq!(popup.scroll_offset, 10, "PageDown moves by 10");

        popup.handle_key(k(KeyCode::PageUp));
        assert_eq!(popup.scroll_offset, 0, "PageUp moves back by 10");
    }

    /// Keys are ignored when the popup is not visible — otherwise stray
    /// arrow keys aimed at the composer would shift a hidden popup's offset.
    #[test]
    fn handle_key_noop_when_not_visible() {
        let mut popup = StatusPopup::new();
        popup.handle_key(k(KeyCode::Down));
        assert_eq!(popup.scroll_offset, 0);
        assert!(!popup.is_visible());
        // Even Esc on a hidden popup must not toggle it visible.
        popup.handle_key(k(KeyCode::Esc));
        assert!(!popup.is_visible());
    }

    /// Left/Right cycle tabs with wraparound at both ends, and the
    /// returned `change` value reports the new tab so the caller can rebuild
    /// the rendered section. The default tab is Overview and the last tab
    /// is History.
    #[test]
    fn left_right_arrows_cycle_tabs_with_wrap() {
        // Right from Overview → Evolution.
        let mut popup = StatusPopup::new();
        popup.visible = true;
        assert_eq!(popup.current_tab(), StatusTab::Overview);
        let change = popup.handle_key(k(KeyCode::Right));
        assert_eq!(change, Some(StatusTab::Evolution));
        assert_eq!(popup.current_tab(), StatusTab::Evolution);

        // Left from Overview wraps to History.
        let mut popup = StatusPopup::new();
        popup.visible = true;
        let change = popup.handle_key(k(KeyCode::Left));
        assert_eq!(change, Some(StatusTab::History));
        assert_eq!(popup.current_tab(), StatusTab::History);

        // Right from History wraps back to Overview.
        let mut popup = StatusPopup::new();
        popup.visible = true;
        popup.current_tab = StatusTab::History;
        popup.handle_key(k(KeyCode::Right));
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
