use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::session::SessionMeta;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

pub enum SessionAction {
    Load { id: String },
}

pub struct SessionsPopup {
    visible: bool,
    sessions: Vec<SessionMeta>,
    cursor: usize,
}

impl SessionsPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            sessions: Vec::new(),
            cursor: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.cursor = 0;
        self.sessions = borg_core::session::list_sessions().unwrap_or_default();
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<SessionAction> {
        use crossterm::event::KeyCode;

        if !self.visible {
            return None;
        }

        match key.code {
            KeyCode::Esc => {
                self.dismiss();
                None
            }
            KeyCode::Up => {
                if !self.sessions.is_empty() {
                    if self.cursor == 0 {
                        self.cursor = self.sessions.len() - 1;
                    } else {
                        self.cursor -= 1;
                    }
                }
                None
            }
            KeyCode::Down => {
                if !self.sessions.is_empty() {
                    self.cursor = (self.cursor + 1) % self.sessions.len();
                }
                None
            }
            KeyCode::Enter => {
                if let Some(session) = self.sessions.get(self.cursor) {
                    let id = session.id.clone();
                    self.dismiss();
                    Some(SessionAction::Load { id })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Sessions", 3, 1)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.sessions.is_empty() {
            lines.push(Line::from(Span::styled(
                " No saved sessions".to_string(),
                theme::dim(),
            )));
        }

        for (i, session) in self.sessions.iter().enumerate() {
            let date = &session.updated_at[..16.min(session.updated_at.len())];
            let title: String = session.title.chars().take(40).collect();
            let label = format!("  {title:<40} {} messages  {date}", session.message_count);

            let style = if i == self.cursor {
                theme::popup_selected()
            } else {
                theme::dim()
            };

            lines.push(Line::from(Span::styled(label, style)));
        }

        let scroll_offset = if self.cursor >= content_height {
            self.cursor - content_height + 1
        } else {
            0
        };

        let visible_lines: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll_offset)
            .take(content_height)
            .collect();

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible_lines), content_area);

        popup_utils::render_footer(frame, inner, " Enter: load  Esc: close");
    }
}

impl PopupHandler for SessionsPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        if let Some(SessionAction::Load { id }) = self.handle_key(key) {
            Ok(Some(AppAction::LoadSession { id }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_session(id: &str, title: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            title: title.to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            message_count: 5,
        }
    }

    fn make_popup_with_sessions(count: usize) -> SessionsPopup {
        let mut popup = SessionsPopup::new();
        popup.visible = true;
        for i in 0..count {
            popup.sessions.push(make_session(
                &format!("session-{i}"),
                &format!("Session {i}"),
            ));
        }
        popup
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn new_popup_not_visible() {
        let popup = SessionsPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = SessionsPopup::new();
        popup.visible = true;
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses() {
        let mut popup = make_popup_with_sessions(2);
        popup.handle_key(key(KeyCode::Esc));
        assert!(!popup.is_visible());
    }

    #[test]
    fn enter_on_empty_returns_none() {
        let mut popup = SessionsPopup::new();
        popup.visible = true;
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_none());
    }

    #[test]
    fn enter_loads_selected_session() {
        let mut popup = make_popup_with_sessions(3);
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_some());
        match result.unwrap() {
            SessionAction::Load { id } => assert_eq!(id, "session-0"),
        }
    }

    #[test]
    fn cursor_wraps_down() {
        let mut popup = make_popup_with_sessions(2);
        assert_eq!(popup.cursor, 0);
        popup.handle_key(key(KeyCode::Down));
        assert_eq!(popup.cursor, 1);
        popup.handle_key(key(KeyCode::Down));
        assert_eq!(popup.cursor, 0); // wrapped
    }

    #[test]
    fn cursor_wraps_up() {
        let mut popup = make_popup_with_sessions(2);
        assert_eq!(popup.cursor, 0);
        popup.handle_key(key(KeyCode::Up));
        assert_eq!(popup.cursor, 1); // wrapped to end
    }

    #[test]
    fn enter_after_navigation_loads_correct_session() {
        let mut popup = make_popup_with_sessions(3);
        popup.handle_key(key(KeyCode::Down)); // cursor -> 1
        let result = popup.handle_key(key(KeyCode::Enter));
        match result.unwrap() {
            SessionAction::Load { id } => assert_eq!(id, "session-1"),
        }
    }

    #[test]
    fn hidden_popup_ignores_keys() {
        let mut popup = make_popup_with_sessions(2);
        popup.visible = false;
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_none());
    }
}
