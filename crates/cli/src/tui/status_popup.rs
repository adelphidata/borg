use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use borg_core::config::Config;

use super::app::{AppAction, PopupHandler};
use super::theme;

pub struct StatusPopup {
    visible: bool,
    scroll_offset: usize,
    lines: Vec<Line<'static>>,
    /// Cached evolution state for rebuilding on resize.
    evo_state: Option<borg_core::evolution::EvolutionState>,
    /// Index in `lines` where the evolution section starts.
    evo_line_start: usize,
    /// Number of lines the evolution section occupies.
    evo_line_count: usize,
    /// Last rendered inner width (for detecting resize).
    last_width: u16,
}

impl StatusPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            scroll_offset: 0,
            lines: Vec::new(),
            evo_state: None,
            evo_line_start: 0,
            evo_line_count: 0,
            last_width: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self, config: &Config) {
        self.visible = true;
        self.scroll_offset = 0;
        self.lines.clear();

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

        let db = match borg_core::db::Database::open() {
            Ok(db) => db,
            Err(_) => {
                self.push_section("Database unavailable.");
                return;
            }
        };

        // Evolution (fetch once, reuse for multiple sections)
        let evo_state = if config.evolution.enabled {
            db.get_evolution_state().ok()
        } else {
            None
        };

        if let Some(ref evo) = evo_state {
            let compact = borg_core::evolution::format_compact(evo);
            self.lines.push(Line::from(vec![
                Span::styled("class: ", theme::dim()),
                Span::styled(compact, Style::default().fg(theme::CYAN)),
            ]));
            self.lines.push(Line::default());
            self.evo_line_start = self.lines.len();
            self.push_section(&borg_core::evolution::format_status_section(evo));
            self.evo_line_count = self.lines.len() - self.evo_line_start;
            self.evo_state = Some(evo.clone());
        }

        // Vitals
        let now = chrono::Utc::now();
        if let Ok(state) = db.get_vitals_state() {
            let state = borg_core::vitals::apply_decay(&state, now);
            let mut drift = borg_core::vitals::detect_drift(&state, now);
            let since = (now - chrono::Duration::days(7)).timestamp();
            let events = db.vitals_events_since(since).unwrap_or_default();
            if borg_core::vitals::detect_failure_drift(&events) {
                drift.push(borg_core::vitals::DriftFlag::RepeatedFailures);
            }
            self.push_section(&borg_core::vitals::format_status(&state, &events, &drift));
        }

        // Bond
        if let Ok(bond_events) = db.get_all_bond_events() {
            let bond_key = db.derive_hmac_key(borg_core::bond::BOND_HMAC_DOMAIN);
            let bond_state = borg_core::bond::replay_events_with_key(&bond_key, &bond_events);
            let correction_rate = borg_core::bond::compute_correction_rate(&db);
            let routine_rate = borg_core::bond::compute_routine_success_rate(&db);
            let pref_count = borg_core::bond::compute_preference_learning_count(&db);
            let since = (now - chrono::Duration::days(7)).timestamp();
            let recent = db.bond_events_since(since).unwrap_or_default();
            self.push_section(&borg_core::bond::format_status(
                &bond_state,
                correction_rate,
                routine_rate,
                pref_count,
                &recent,
            ));
        }

        // Archetype scores
        if let Some(ref evo) = evo_state {
            self.push_section(&borg_core::evolution::format_archetype_scores(evo));
        }

        // Evolution history
        if config.evolution.enabled {
            if let Ok(events) = db.evolution_events_since(0) {
                let mut events = events;
                events.reverse();
                self.push_section(&borg_core::evolution::format_history(&events));
            }
        }
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if !self.visible {
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.dismiss(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            _ => {}
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

        if inner.height < 3 || inner.width < 10 {
            return;
        }

        // Rebuild evolution card section if width changed
        if inner.width != self.last_width {
            self.last_width = inner.width;
            let rebuilt = self.evo_state.as_ref().map(|evo| {
                // Card width = inner width minus 2 chars indent on each side
                let card_width = (inner.width as usize).saturating_sub(4);
                let section =
                    borg_core::evolution::format_status_section_with_width(evo, card_width);
                section
                    .lines()
                    .map(|l| Line::from(l.to_string()))
                    .chain(std::iter::once(Line::default()))
                    .collect::<Vec<Line<'static>>>()
            });
            if let Some(new_lines) = rebuilt {
                let end = (self.evo_line_start + self.evo_line_count).min(self.lines.len());
                let count = new_lines.len();
                self.lines.splice(self.evo_line_start..end, new_lines);
                self.evo_line_count = count;
            }
        }

        let max_scroll = self.lines.len().saturating_sub(inner.height as usize);
        let offset = self.scroll_offset.min(max_scroll);

        let paragraph = Paragraph::new(self.lines.clone()).scroll((offset as u16, 0));

        frame.render_widget(paragraph, inner);

        // Footer hint
        let footer_y = popup_area.y + popup_area.height.saturating_sub(1);
        if popup_area.width > 20 {
            let hint = " Esc: close  \u{2191}\u{2193}: scroll ";
            let hint_x = popup_area.x + 2;
            let hint_area = Rect::new(hint_x, footer_y, hint.len() as u16, 1);
            let hint_widget = Paragraph::new(Line::from(Span::styled(hint, theme::dim())));
            frame.render_widget(hint_widget, hint_area);
        }
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
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        self.handle_key(key);
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
}
