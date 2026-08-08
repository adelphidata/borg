use std::sync::Arc;

use super::custom_terminal::Frame;
use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::sync::Mutex;

use cortex::config::Config;
use protocols::admin::SessionSummary;

use crate::daemon_client::DaemonClient;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

#[derive(Debug)]
pub enum SessionAction {
    Load {
        id: String,
    },
    Export {
        id: String,
        format: cortex::export::ExportFormat,
    },
}

/// Export formats offered by the popup, in `f`-key cycle order.
const EXPORT_FORMATS: &[cortex::export::ExportFormat] = &[
    cortex::export::ExportFormat::Json,
    cortex::export::ExportFormat::Csv,
    cortex::export::ExportFormat::Txt,
    cortex::export::ExportFormat::Markdown,
];

pub struct SessionsPopup {
    visible: bool,
    sessions: Vec<SessionSummary>,
    cursor: usize,
    /// Index into [`EXPORT_FORMATS`] selecting the `e`-key export format.
    format_idx: usize,
}

impl SessionsPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            sessions: Vec::new(),
            cursor: 0,
            format_idx: 0,
        }
    }

    /// Export format currently selected via the `f` key (default JSON).
    fn selected_format(&self) -> cortex::export::ExportFormat {
        EXPORT_FORMATS[self.format_idx]
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self, client: &Arc<Mutex<DaemonClient>>) {
        self.visible = true;
        self.cursor = 0;
        self.sessions = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut c = client.lock().await;
                match c.list_sessions().await {
                    Ok(list) => list.sessions,
                    Err(e) => {
                        tracing::warn!("SessionsPopup: list_sessions failed: {e}");
                        Vec::new()
                    }
                }
            })
        });
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
            KeyCode::Char('e') => {
                if let Some(session) = self.sessions.get(self.cursor) {
                    let id = session.id.clone();
                    let format = self.selected_format();
                    self.dismiss();
                    Some(SessionAction::Export { id, format })
                } else {
                    None
                }
            }
            KeyCode::Char('f') => {
                self.format_idx = (self.format_idx + 1) % EXPORT_FORMATS.len();
                None
            }
            _ => None,
        }
    }

    /// One listing row: pin marker, truncated title, message count, date.
    /// Extracted from `render` so the pin indicator is unit-testable.
    fn row_label(session: &SessionSummary) -> String {
        let date = chrono::DateTime::from_timestamp(session.updated_unix_ts as i64, 0)
            .map(|dt| dt.format(crate::commands::TS_FMT).to_string())
            .unwrap_or_default();
        let title: String = session.title.chars().take(40).collect();
        let pin = if session.pinned { '*' } else { ' ' };
        format!(
            " {pin} {title:<40} {} messages  {date}",
            session.message_count
        )
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
            let label = Self::row_label(session);

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

        let footer = format!(
            " Enter: load  e: export ({})  f: format  Esc: close",
            self.selected_format().as_str()
        );
        popup_utils::render_footer(frame, inner, &footer);
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
        match self.handle_key(key) {
            Some(SessionAction::Load { id }) => Ok(Some(AppAction::LoadSession { id })),
            Some(SessionAction::Export { id, format }) => {
                Ok(Some(AppAction::ExportSession { id, format }))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
