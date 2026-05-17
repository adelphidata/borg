//! Subprocess registry view — running children with inline pin toggle.
//!
//! Mirrors `SessionsPopup`'s cursor-list pattern. The single non-obvious
//! behavior is `p` to flip the auto-reap pin on the highlighted entry: a
//! synchronous Admin RPC fires, the snapshot reloads on success, and the
//! cursor stays put so the operator can scan and toggle several entries
//! without losing context.
//!
//! Finished entries are listed below running entries, dimmed, in
//! most-recently-completed order. They're informational only — no
//! actions apply to them, and the cursor skips over them.

use std::sync::Arc;

use super::custom_terminal::Frame;
use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio::sync::Mutex;

use cortex::config::Config;
use protocols::admin::{Subprocess, SubprocessSnapshot};

use crate::daemon_client::DaemonClient;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

/// Outcome of a key press on the popup. App-level code converts this
/// into the right side-effect (toggle pin RPC, reload RPC, dismiss).
/// Split out from `handle_key` so the pure navigation arms can be
/// tested without needing a `DaemonClient` handle.
#[derive(Debug, PartialEq, Eq)]
pub enum SubprocessAction {
    None,
    Dismiss,
    /// Pin/unpin the currently-highlighted running entry.
    TogglePin {
        id: u64,
        currently_pinned: bool,
    },
    Reload,
}

pub struct SubprocessesPopup {
    visible: bool,
    snapshot: SubprocessSnapshot,
    /// Cursor index into the `running` slice. Finished entries are not
    /// selectable since pinning a finished entry is a no-op.
    cursor: usize,
    /// Last-action status line shown above the footer. Cleared on the
    /// next render of a new snapshot.
    status: Option<(String, bool)>,
    /// Daemon client cached at `show()` time so `p` / `r` key handlers
    /// can drive the RPC without App-level routing. None in tests that
    /// poke the popup via direct field access.
    client: Option<Arc<Mutex<DaemonClient>>>,
}

impl SubprocessesPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            snapshot: SubprocessSnapshot::default(),
            cursor: 0,
            status: None,
            client: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Open the popup and refresh the snapshot from the daemon.
    pub fn show(&mut self, client: &Arc<Mutex<DaemonClient>>) {
        self.visible = true;
        self.cursor = 0;
        self.status = None;
        self.client = Some(client.clone());
        self.reload(client);
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    fn reload(&mut self, client: &Arc<Mutex<DaemonClient>>) {
        let snap = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut c = client.lock().await;
                match c.get_subprocesses().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("SubprocessesPopup: get_subprocesses failed: {e}");
                        SubprocessSnapshot::default()
                    }
                }
            })
        });
        self.snapshot = snap;
        if self.cursor >= self.snapshot.running.len() {
            self.cursor = self.snapshot.running.len().saturating_sub(1);
        }
    }

    fn highlighted_running(&self) -> Option<&Subprocess> {
        self.snapshot.running.get(self.cursor)
    }

    /// Apply the outcome of a successful pin toggle. Called by App after
    /// the RPC returns Ok; the popup updates its status line and waits
    /// for the next snapshot refresh (which App also triggers).
    pub fn record_pin_success(&mut self, id: u64, now_pinned: bool) {
        self.status = Some((
            if now_pinned {
                format!("Pinned #{id}")
            } else {
                format!("Unpinned #{id}")
            },
            true,
        ));
    }

    /// Record a failed pin toggle so the operator sees feedback.
    pub fn record_pin_failure(&mut self, err: &str) {
        self.status = Some((format!("Pin toggle failed: {err}"), false));
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> SubprocessAction {
        use crossterm::event::KeyCode;
        if !self.visible {
            return SubprocessAction::None;
        }
        match key.code {
            KeyCode::Esc => {
                self.dismiss();
                SubprocessAction::Dismiss
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let n = self.snapshot.running.len();
                if n > 0 {
                    self.cursor = if self.cursor == 0 {
                        n - 1
                    } else {
                        self.cursor - 1
                    };
                }
                SubprocessAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let n = self.snapshot.running.len();
                if n > 0 {
                    self.cursor = (self.cursor + 1) % n;
                }
                SubprocessAction::None
            }
            KeyCode::Char('p') => match self.highlighted_running() {
                Some(entry) => SubprocessAction::TogglePin {
                    id: entry.id,
                    currently_pinned: entry.pinned,
                },
                None => SubprocessAction::None,
            },
            KeyCode::Char('r') => SubprocessAction::Reload,
            _ => SubprocessAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Subprocesses", 3, 2)
        else {
            return;
        };

        let now = chrono::Utc::now().timestamp();
        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.snapshot.running.is_empty() && self.snapshot.finished.is_empty() {
            lines.push(Line::from(Span::styled(
                " No tracked subprocesses".to_string(),
                theme::dim(),
            )));
        }

        if !self.snapshot.running.is_empty() {
            lines.push(Line::from(Span::styled(
                " Running".to_string(),
                theme::dim(),
            )));
        }

        for (i, entry) in self.snapshot.running.iter().enumerate() {
            let label = format_subprocess_line(entry, now, false);
            let style = if i == self.cursor {
                theme::popup_selected()
            } else {
                theme::dim()
            };
            lines.push(Line::from(Span::styled(label, style)));
        }

        if !self.snapshot.finished.is_empty() {
            lines.push(Line::from(Span::styled(
                " Recently finished".to_string(),
                theme::dim(),
            )));
            for entry in self.snapshot.finished.iter().take(10) {
                let label = format_subprocess_line(entry, now, true);
                lines.push(Line::from(Span::styled(label, theme::dim())));
            }
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

        popup_utils::render_status_message(frame, inner, self.status.as_ref(), 2);
        popup_utils::render_footer(frame, inner, " p: toggle pin  r: reload  Esc: close");
    }
}

fn format_subprocess_line(entry: &Subprocess, now: i64, finished: bool) -> String {
    let pin_marker = if entry.pinned { "📌" } else { "  " };
    let age = format_age(now.saturating_sub(entry.started_at_unix));
    let command: String = entry.command.chars().take(60).collect();
    let kind = format!("[{}]", entry.kind);
    if finished {
        let status_str: &str = entry.status.as_str();
        format!("    {kind:<18} {command:<60} {age:>6}  {status_str}")
    } else {
        format!(" {pin_marker} {kind:<18} {command:<60} {age:>6}")
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs.max(0))
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

impl PopupHandler for SubprocessesPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        let action = self.handle_key(key);
        match action {
            SubprocessAction::None => Ok(None),
            SubprocessAction::Dismiss => Ok(Some(AppAction::LeaveAltScreen)),
            SubprocessAction::TogglePin {
                id,
                currently_pinned,
            } => {
                let Some(client) = self.client.clone() else {
                    return Ok(None);
                };
                let new_pinned = !currently_pinned;
                let rpc_result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let mut c = client.lock().await;
                        c.set_subprocess_pinned(id, new_pinned).await
                    })
                });
                match rpc_result {
                    Ok(()) => {
                        self.record_pin_success(id, new_pinned);
                        self.reload(&client);
                    }
                    Err(e) => self.record_pin_failure(&e.to_string()),
                }
                Ok(None)
            }
            SubprocessAction::Reload => {
                if let Some(client) = self.client.clone() {
                    self.reload(&client);
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_subprocess(id: u64, command: &str, pinned: bool) -> Subprocess {
        Subprocess {
            id,
            pid: 1000 + id as u32,
            kind: "RunShell".to_string(),
            command: command.to_string(),
            session_id: String::new(),
            started_at_unix: 0,
            finished_at_unix: 0,
            finished: false,
            exit_code: 0,
            exit_code_present: false,
            status: "Running".to_string(),
            pinned,
        }
    }

    fn make_popup(running: Vec<Subprocess>) -> SubprocessesPopup {
        let mut popup = SubprocessesPopup::new();
        popup.visible = true;
        popup.snapshot = SubprocessSnapshot {
            running,
            finished: Vec::new(),
        };
        popup
    }

    #[test]
    fn esc_dismisses_popup() {
        let mut popup = make_popup(vec![make_subprocess(1, "echo hi", false)]);
        let action = popup.handle_key(key(KeyCode::Esc));
        assert_eq!(action, SubprocessAction::Dismiss);
        assert!(!popup.is_visible());
    }

    /// Arrow keys + vim `j`/`k` all navigate the same cursor and wrap at both
    /// ends so the popup never lands on an out-of-range row.
    #[test]
    fn navigation_keys_wrap_at_both_ends() {
        for &(down_key, up_key) in &[
            (KeyCode::Down, KeyCode::Up),
            (KeyCode::Char('j'), KeyCode::Char('k')),
        ] {
            let mut popup = make_popup(vec![
                make_subprocess(1, "a", false),
                make_subprocess(2, "b", false),
            ]);
            assert_eq!(popup.cursor, 0, "fresh cursor with key {down_key:?}");
            popup.handle_key(key(down_key));
            assert_eq!(popup.cursor, 1, "after one {down_key:?}");
            popup.handle_key(key(down_key));
            assert_eq!(popup.cursor, 0, "after wrap with {down_key:?}");
            // Wrap upward: from 0 jumps to the last entry.
            popup.handle_key(key(up_key));
            assert_eq!(popup.cursor, 1, "after wrap-up with {up_key:?}");
        }
    }

    #[test]
    fn empty_running_list_does_not_panic_on_navigation() {
        let mut popup = make_popup(vec![]);
        popup.handle_key(key(KeyCode::Down));
        popup.handle_key(key(KeyCode::Up));
        assert_eq!(popup.cursor, 0);
    }

    #[test]
    fn hidden_popup_returns_none_action() {
        let mut popup = make_popup(vec![make_subprocess(1, "a", false)]);
        popup.visible = false;
        assert_eq!(popup.handle_key(key(KeyCode::Esc)), SubprocessAction::None);
    }

    #[test]
    fn p_returns_toggle_pin_with_current_state() {
        let mut popup = make_popup(vec![
            make_subprocess(1, "unpinned", false),
            make_subprocess(2, "pinned", true),
        ]);
        // cursor at 0 → unpinned entry
        assert_eq!(
            popup.handle_key(key(KeyCode::Char('p'))),
            SubprocessAction::TogglePin {
                id: 1,
                currently_pinned: false,
            }
        );
        // move to second (pinned) entry
        popup.handle_key(key(KeyCode::Down));
        assert_eq!(
            popup.handle_key(key(KeyCode::Char('p'))),
            SubprocessAction::TogglePin {
                id: 2,
                currently_pinned: true,
            }
        );
    }

    #[test]
    fn p_with_no_running_entries_is_a_noop() {
        let mut popup = make_popup(vec![]);
        assert_eq!(
            popup.handle_key(key(KeyCode::Char('p'))),
            SubprocessAction::None
        );
    }

    #[test]
    fn r_returns_reload_action() {
        let mut popup = make_popup(vec![make_subprocess(1, "a", false)]);
        assert_eq!(
            popup.handle_key(key(KeyCode::Char('r'))),
            SubprocessAction::Reload
        );
    }

    /// `record_pin_*` sets the status line in `popup.status` so the next
    /// frame surfaces an "ok"/"error" toast under the entry list.
    #[test]
    fn record_pin_outcome_sets_status_with_success_flag() {
        let mut popup = make_popup(vec![make_subprocess(1, "a", false)]);
        popup.record_pin_success(1, true);
        assert!(matches!(popup.status, Some((_, true))));

        popup.record_pin_failure("boom");
        assert!(
            matches!(&popup.status, Some((msg, false)) if msg.contains("boom")),
            "failure status must carry the error text, got {:?}",
            popup.status
        );
    }

    #[test]
    fn format_age_chooses_unit_by_magnitude() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3_599), "59m");
        assert_eq!(format_age(3_600), "1h");
        assert_eq!(format_age(86_399), "23h");
        assert_eq!(format_age(86_400), "1d");
    }

    #[test]
    fn format_age_clamps_negative_to_zero() {
        // A clock skew between daemon-recorded started_at and the local
        // `now` would produce a negative delta; render as 0s rather than
        // a garbage negative number.
        assert_eq!(format_age(-5), "0s");
    }

    /// The pin marker is the user's only visual cue that a subprocess is
    /// surviving daemon restart, so the glyph must appear exactly when the
    /// `pinned` flag is set.
    #[test]
    fn format_subprocess_line_shows_pin_marker_iff_pinned() {
        let pinned = format_subprocess_line(&make_subprocess(1, "cargo build", true), 0, false);
        let unpinned = format_subprocess_line(&make_subprocess(1, "cargo build", false), 0, false);
        assert!(pinned.contains("📌"), "missing pin marker: {pinned}");
        assert!(!unpinned.contains("📌"), "stray pin marker: {unpinned}");
    }
}
