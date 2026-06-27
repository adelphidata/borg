//! `/usage` popup — renders the daemon-formatted text view in an
//! alt-screen popup. The daemon owns the formatter (mirroring the
//! `StatusPopup` pattern with `GetXpText` / `GetEvolutionText`); this
//! popup just blocks on the RPC and paints the returned string.

use std::sync::Arc;

use super::custom_terminal::Frame;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::sync::Mutex;

use cortex::config::Config;

use crate::daemon_client::DaemonClient;

use super::app::{AppAction, PopupHandler};
use super::theme;

pub struct UsagePopup {
    visible: bool,
    scroll_offset: usize,
    lines: Vec<Line<'static>>,
    /// Window the popup last fetched at, in days. Stored so the title bar
    /// can show the active window even after a `reload()`. `None` means
    /// the popup has never been opened in this process.
    days: Option<u32>,
    /// Daemon client for the `GetUsageText` RPC.
    client: Option<Arc<Mutex<DaemonClient>>>,
}

impl UsagePopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            scroll_offset: 0,
            lines: Vec::new(),
            days: None,
            client: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Open the popup over a `days`-day rolling window. `days = 0` lets
    /// the daemon use its default (30); the daemon also clamps oversized
    /// values, so the cli does not need to validate.
    pub fn show(&mut self, days: u32, client: &Arc<Mutex<DaemonClient>>) {
        self.visible = true;
        self.days = Some(days);
        self.client = Some(client.clone());
        self.reload();
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    fn reload(&mut self) {
        self.scroll_offset = 0;
        self.lines.clear();
        let days = self.days.unwrap_or(0);
        let snap = self.block_on_rpc(move |c| Box::pin(c.get_usage_text(days)));
        match snap {
            Ok(text) => {
                for line in text.text.lines() {
                    self.lines.push(Line::from(line.to_string()));
                }
            }
            Err(e) => {
                tracing::warn!("usage_popup: GetUsageText RPC failed: {e:#}");
                self.lines
                    .push(Line::from("Usage unavailable.".to_string()));
            }
        }
    }

    fn block_on_rpc<F, T>(&self, f: F) -> Result<T>
    where
        F: for<'b> FnOnce(
            &'b mut DaemonClient,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T>> + Send + 'b>,
        >,
    {
        let client = self
            .client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("daemon client not attached"))?;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| anyhow::anyhow!("no tokio runtime: {e}"))?;
        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                let mut c = client.lock().await;
                f(&mut c).await
            })
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        if !self.visible {
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.dismiss();
                Some(AppAction::Continue)
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
            KeyCode::Home => {
                self.scroll_offset = 0;
                None
            }
            KeyCode::End => {
                self.scroll_offset = self.lines.len().saturating_sub(1);
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

        let title = match self.days {
            Some(0) | None => "Usage".to_string(),
            Some(d) => format!("Usage · {d}d"),
        };
        let inner = super::popup_utils::render_popup_frame(frame, popup_area, &title);

        if inner.height < 2 || inner.width < 10 {
            return;
        }

        let max_scroll = self.lines.len().saturating_sub(inner.height as usize);
        let offset = self.scroll_offset.min(max_scroll);
        let paragraph = Paragraph::new(self.lines.clone()).scroll((offset as u16, 0));
        frame.render_widget(paragraph, inner);

        // Footer hint
        let footer_y = popup_area.y + popup_area.height.saturating_sub(1);
        if popup_area.width > 40 {
            let hint = " Esc/q: close  \u{2191}\u{2193}: scroll ";
            let hint_x = popup_area.x + 2;
            let hint_area = Rect::new(hint_x, footer_y, hint.len() as u16, 1);
            let hint_widget = Paragraph::new(Line::from(Span::styled(hint, theme::dim())));
            frame.render_widget(hint_widget, hint_area);
        }
    }
}

impl PopupHandler for UsagePopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        Ok(self.handle_key(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_popup_not_visible() {
        let popup = UsagePopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = UsagePopup::new();
        popup.visible = true;
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses_when_visible() {
        let mut popup = UsagePopup::new();
        popup.visible = true;
        let action = popup.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(action, Some(AppAction::Continue)));
        assert!(!popup.is_visible());
    }

    #[test]
    fn keys_no_op_when_invisible() {
        let mut popup = UsagePopup::new();
        // Not visible: even Esc should be a no-op so global handlers can run.
        let action = popup.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(action.is_none());
    }
}
