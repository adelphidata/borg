//! `/btw` popup — renders the answer (or loading / error state) for an
//! in-flight ephemeral side question. Dismissable with Esc; does not mutate
//! `AppState`, so the main agent turn can keep streaming underneath.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use borg_core::config::Config;

use super::app::{AppAction, PopupHandler};
use super::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtwState {
    Hidden,
    Loading { question: String },
    Ready { question: String, answer: String },
    Error { question: String, error: String },
}

pub struct BtwPopup {
    state: BtwState,
    scroll_offset: u16,
}

impl BtwPopup {
    pub fn new() -> Self {
        Self {
            state: BtwState::Hidden,
            scroll_offset: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        !matches!(self.state, BtwState::Hidden)
    }

    /// Borrow the current state — used by the event loop to decide whether
    /// a late-arriving `BtwResult` still matches the popup's in-flight
    /// question (vs. having been dismissed or superseded).
    pub fn state(&self) -> &BtwState {
        &self.state
    }

    pub fn show_loading(&mut self, question: String) {
        self.scroll_offset = 0;
        self.state = BtwState::Loading { question };
    }

    pub fn show_ready(&mut self, question: String, answer: String) {
        self.scroll_offset = 0;
        self.state = BtwState::Ready { question, answer };
    }

    pub fn show_error(&mut self, question: String, error: String) {
        self.scroll_offset = 0;
        self.state = BtwState::Error { question, error };
    }

    pub fn dismiss(&mut self) {
        self.state = BtwState::Hidden;
        self.scroll_offset = 0;
    }

    pub fn render(&mut self, frame: &mut Frame) {
        if !self.is_visible() {
            return;
        }

        let full = frame.area();
        // Centered modal: 70% width, 60% height, min 40x10.
        let w = ((full.width as u32 * 70) / 100).max(40) as u16;
        let h = ((full.height as u32 * 60) / 100).max(10) as u16;
        let w = w.min(full.width);
        let h = h.min(full.height);
        let x = full.x + (full.width.saturating_sub(w)) / 2;
        let y = full.y + (full.height.saturating_sub(h)) / 2;
        let area = Rect::new(x, y, w, h);

        frame.render_widget(Clear, area);

        let title_style = Style::default()
            .fg(theme::CYAN)
            .add_modifier(Modifier::BOLD);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(Span::styled(" /btw ", title_style));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 3 || inner.width < 10 {
            return;
        }

        let (question, body_lines, footer_hint) = match &self.state {
            BtwState::Hidden => return,
            BtwState::Loading { question } => (
                question.as_str(),
                vec![Line::from(Span::styled(
                    "thinking…",
                    theme::dim().add_modifier(Modifier::ITALIC),
                ))],
                " Esc: dismiss ",
            ),
            BtwState::Ready { question, answer } => (
                question.as_str(),
                answer
                    .lines()
                    .map(|l| Line::from(l.to_string()))
                    .collect::<Vec<_>>(),
                " Esc: dismiss  \u{2191}\u{2193}: scroll ",
            ),
            BtwState::Error { question, error } => (
                question.as_str(),
                vec![Line::from(Span::styled(
                    format!("error: {error}"),
                    theme::error_style(),
                ))],
                " Esc: dismiss ",
            ),
        };

        // Question row (top), then separator, then body.
        let q_line = Line::from(vec![
            Span::styled("Q: ", theme::dim()),
            Span::styled(
                question.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]);
        let q_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(q_line).wrap(Wrap { trim: false }), q_area);

        if inner.height >= 3 {
            let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(inner.width as usize),
                    theme::dim(),
                ))),
                sep_area,
            );
        }

        let body_top = inner.y + 2;
        let body_height = inner.height.saturating_sub(2);
        if body_height > 0 {
            let body_area = Rect::new(inner.x, body_top, inner.width, body_height);
            let paragraph = Paragraph::new(body_lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll_offset, 0));
            frame.render_widget(paragraph, body_area);
        }

        // Footer hint — bottom border row.
        if area.width > footer_hint.len() as u16 + 4 {
            let footer_y = area.y + area.height.saturating_sub(1);
            let footer_area = Rect::new(area.x + 2, footer_y, footer_hint.len() as u16, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(footer_hint, theme::dim()))),
                footer_area,
            );
        }
    }
}

impl Default for BtwPopup {
    fn default() -> Self {
        Self::new()
    }
}

impl PopupHandler for BtwPopup {
    fn is_visible(&self) -> bool {
        BtwPopup::is_visible(self)
    }

    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        match key.code {
            KeyCode::Esc => {
                self.dismiss();
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                Ok(Some(AppAction::Continue))
            }
            // Absorb everything else so keys don't leak to the composer.
            _ => Ok(Some(AppAction::Continue)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn hidden_by_default() {
        let p = BtwPopup::new();
        assert!(!p.is_visible());
        assert_eq!(p.state(), &BtwState::Hidden);
    }

    #[test]
    fn show_loading_then_ready_transitions_state() {
        let mut p = BtwPopup::new();
        p.show_loading("what?".to_string());
        assert!(matches!(p.state(), BtwState::Loading { .. }));
        p.show_ready("what?".to_string(), "answer".to_string());
        match p.state() {
            BtwState::Ready { question, answer } => {
                assert_eq!(question, "what?");
                assert_eq!(answer, "answer");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses_and_returns_continue() {
        let mut p = BtwPopup::new();
        p.show_ready("q".to_string(), "a".to_string());
        let mut cfg = Config::default();
        let action = p
            .handle_key_event(k(KeyCode::Esc), &mut cfg)
            .expect("key handler ok");
        assert!(matches!(action, Some(AppAction::Continue)));
        assert!(!p.is_visible());
        assert_eq!(p.state(), &BtwState::Hidden);
    }

    #[test]
    fn non_dismiss_keys_are_absorbed_and_do_not_change_visibility() {
        // Ensures typed characters can't leak to the composer while the popup
        // is open — the precondition for the "feels modal" UX.
        let mut p = BtwPopup::new();
        p.show_loading("q".to_string());
        let mut cfg = Config::default();
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('/'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
        ] {
            let action = p.handle_key_event(k(code), &mut cfg).unwrap();
            assert!(
                matches!(action, Some(AppAction::Continue)),
                "key {code:?} must be absorbed as Continue"
            );
            assert!(p.is_visible(), "key {code:?} must not dismiss the popup");
        }
    }

    #[test]
    fn arrow_keys_scroll_without_overflow() {
        let mut p = BtwPopup::new();
        p.show_ready("q".to_string(), "line\n".repeat(5));
        let mut cfg = Config::default();
        // Up from zero must saturate, not underflow.
        let _ = p.handle_key_event(k(KeyCode::Up), &mut cfg).unwrap();
        assert_eq!(p.scroll_offset, 0);
        let _ = p.handle_key_event(k(KeyCode::Down), &mut cfg).unwrap();
        let _ = p.handle_key_event(k(KeyCode::Down), &mut cfg).unwrap();
        assert_eq!(p.scroll_offset, 2);
        let _ = p.handle_key_event(k(KeyCode::Up), &mut cfg).unwrap();
        assert_eq!(p.scroll_offset, 1);
    }

    #[test]
    fn dismiss_resets_scroll_offset() {
        // Regression: if a user scrolled the previous answer, the next
        // `/btw` must start from the top, not inherit the old offset.
        let mut p = BtwPopup::new();
        p.show_ready("q1".to_string(), "long answer".to_string());
        let mut cfg = Config::default();
        for _ in 0..3 {
            let _ = p.handle_key_event(k(KeyCode::Down), &mut cfg).unwrap();
        }
        assert_eq!(p.scroll_offset, 3);
        p.dismiss();
        p.show_loading("q2".to_string());
        assert_eq!(p.scroll_offset, 0);
    }
}
