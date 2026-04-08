use std::collections::HashSet;
use std::path::Path;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::db::{ApprovedSenderRow, PairingRequestRow};
use borg_core::pairing::channel_display_name;
use borg_plugins::catalog::CATALOG;
use borg_plugins::installer::is_installed;

use super::popup_utils;
use super::theme;

/// A unified item in the pairing popup list.
enum PairingItem {
    Pending {
        request: PairingRequestRow,
    },
    Approved {
        sender: ApprovedSenderRow,
        pending_revoke: bool,
    },
    Unpaired {
        display_name: String,
    },
}

/// Actions returned to the event loop for execution.
pub enum PairingAction {
    Approve { channel: String, code: String },
    Revoke { channel: String, sender_id: String },
}

pub struct PairingPopup {
    visible: bool,
    items: Vec<PairingItem>,
    cursor: usize,
    status_message: Option<(String, bool)>,
}

impl PairingPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            items: Vec::new(),
            cursor: 0,
            status_message: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self, _config: &Config, data_dir: &Path) {
        self.visible = true;
        self.cursor = 0;
        self.status_message = None;
        self.items.clear();

        let db = match borg_core::db::Database::open() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("Failed to open database for pairing popup: {e}");
                return;
            }
        };

        let pending = db.list_pairings(None).unwrap_or_default();
        let approved = db.list_approved_senders(None).unwrap_or_default();

        // Collect channel names that have pending or approved entries
        let active_channels: HashSet<String> = pending
            .iter()
            .map(|r| r.channel_name.clone())
            .chain(approved.iter().map(|s| s.channel_name.clone()))
            .collect();

        // Pending items first
        for request in pending {
            self.items.push(PairingItem::Pending { request });
        }

        // Approved items
        for sender in approved {
            self.items.push(PairingItem::Approved {
                sender,
                pending_revoke: false,
            });
        }

        // Unpaired: installed channels with no pending/approved entries
        for def in CATALOG {
            if !is_installed(def, data_dir) {
                continue;
            }
            let channel_name = channel_name_from_plugin_id(def.id);
            if !active_channels.contains(&channel_name) {
                let display_name = channel_display_name(&channel_name);
                self.items.push(PairingItem::Unpaired { display_name });
            }
        }

        // Clamp cursor
        if !self.items.is_empty() && self.cursor >= self.items.len() {
            self.cursor = self.items.len() - 1;
        }
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.status_message = None;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Vec<PairingAction>> {
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
                if !self.items.is_empty() {
                    if self.cursor == 0 {
                        self.cursor = self.items.len() - 1;
                    } else {
                        self.cursor -= 1;
                    }
                    self.status_message = None;
                }
                None
            }
            KeyCode::Down => {
                if !self.items.is_empty() {
                    self.cursor = (self.cursor + 1) % self.items.len();
                    self.status_message = None;
                }
                None
            }
            KeyCode::Char('d') => {
                if let Some(PairingItem::Approved { pending_revoke, .. }) =
                    self.items.get_mut(self.cursor)
                {
                    *pending_revoke = !*pending_revoke;
                    self.status_message = None;
                }
                None
            }
            KeyCode::Char('a') => {
                // Approve the selected pending item
                if let Some(PairingItem::Pending { request }) = self.items.get(self.cursor) {
                    let actions = vec![PairingAction::Approve {
                        channel: request.channel_name.clone(),
                        code: request.code.clone(),
                    }];
                    self.dismiss();
                    return Some(actions);
                }
                None
            }
            KeyCode::Enter => {
                // If cursor is on a pending item, approve it
                if let Some(PairingItem::Pending { request }) = self.items.get(self.cursor) {
                    let actions = vec![PairingAction::Approve {
                        channel: request.channel_name.clone(),
                        code: request.code.clone(),
                    }];
                    self.dismiss();
                    return Some(actions);
                }

                // If cursor is on an unpaired item, show instructions
                if let Some(PairingItem::Unpaired { display_name }) = self.items.get(self.cursor) {
                    self.status_message = Some((
                        format!("Send a message to your {display_name} bot to start pairing"),
                        true,
                    ));
                    return None;
                }

                // Collect all pending revokes
                let actions: Vec<PairingAction> = self
                    .items
                    .iter()
                    .filter_map(|item| {
                        if let PairingItem::Approved {
                            sender,
                            pending_revoke: true,
                        } = item
                        {
                            Some(PairingAction::Revoke {
                                channel: sender.channel_name.clone(),
                                sender_id: sender.sender_id.clone(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                if actions.is_empty() {
                    self.status_message = Some(("No changes to apply".to_string(), false));
                    return None;
                }

                self.dismiss();
                Some(actions)
            }
            _ => None,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Pairing", 5, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.items.is_empty() {
            lines.push(Line::from(Span::styled(
                " No pairing data. Enable a channel in /plugins first.".to_string(),
                theme::dim(),
            )));
        }

        for (i, item) in self.items.iter().enumerate() {
            let is_selected = i == self.cursor;

            let (badge, label, style) = match item {
                PairingItem::Pending { request } => {
                    let display = channel_display_name(&request.channel_name);
                    let label =
                        format!("  {:<10} {} | {}", display, request.sender_id, request.code);
                    let style = if is_selected {
                        theme::popup_selected()
                    } else {
                        theme::dim()
                    };
                    ("[pending] ", label, style)
                }
                PairingItem::Approved {
                    sender,
                    pending_revoke,
                } => {
                    let display = channel_display_name(&sender.channel_name);
                    let name = sender.display_name.as_deref().unwrap_or("");
                    let label = if name.is_empty() {
                        format!("  {:<10} {}", display, sender.sender_id)
                    } else {
                        format!("  {:<10} {} ({})", display, sender.sender_id, name)
                    };
                    if *pending_revoke {
                        ("[revoke]  ", label, theme::error_style())
                    } else {
                        let style = if is_selected {
                            theme::popup_selected()
                        } else {
                            theme::dim()
                        };
                        ("[approved]", label, style)
                    }
                }
                PairingItem::Unpaired { display_name } => {
                    let label = format!("  {display_name} — no paired sender");
                    let style = if is_selected {
                        theme::popup_selected()
                    } else {
                        theme::dim()
                    };
                    ("[unpaired]", label, style)
                }
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {badge} "), style),
                Span::styled(label, style),
            ]));
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

        popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);
        popup_utils::render_footer(
            frame,
            inner,
            " a: approve  d: revoke  Enter: apply  Esc: close",
        );
    }
}

/// Extract the channel name from a plugin catalog ID like "messaging/telegram" → "telegram".
fn channel_name_from_plugin_id(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_pending(channel: &str, sender: &str, code: &str) -> PairingItem {
        PairingItem::Pending {
            request: PairingRequestRow {
                id: format!("req-{code}"),
                channel_name: channel.to_string(),
                sender_id: sender.to_string(),
                code: code.to_string(),
                status: "pending".to_string(),
                display_name: None,
                created_at: 1000,
                expires_at: 9999999,
                approved_at: None,
            },
        }
    }

    fn make_approved(channel: &str, sender: &str) -> PairingItem {
        PairingItem::Approved {
            sender: ApprovedSenderRow {
                id: 1,
                channel_name: channel.to_string(),
                sender_id: sender.to_string(),
                display_name: None,
                approved_at: 1000,
            },
            pending_revoke: false,
        }
    }

    fn make_unpaired(channel: &str) -> PairingItem {
        PairingItem::Unpaired {
            display_name: channel_display_name(channel),
        }
    }

    fn make_popup(items: Vec<PairingItem>) -> PairingPopup {
        PairingPopup {
            visible: true,
            items,
            cursor: 0,
            status_message: None,
        }
    }

    #[test]
    fn new_popup_not_visible() {
        let popup = PairingPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = PairingPopup::new();
        popup.visible = true;
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses() {
        let mut popup = make_popup(vec![make_pending("telegram", "123", "TG_ABC")]);
        popup.handle_key(key(KeyCode::Esc));
        assert!(!popup.is_visible());
    }

    #[test]
    fn cursor_wraps_down() {
        let mut popup = make_popup(vec![
            make_pending("telegram", "123", "TG_ABC"),
            make_approved("discord", "456"),
        ]);
        assert_eq!(popup.cursor, 0);
        popup.handle_key(key(KeyCode::Down));
        assert_eq!(popup.cursor, 1);
        popup.handle_key(key(KeyCode::Down));
        assert_eq!(popup.cursor, 0); // wrapped
    }

    #[test]
    fn cursor_wraps_up() {
        let mut popup = make_popup(vec![
            make_pending("telegram", "123", "TG_ABC"),
            make_approved("discord", "456"),
        ]);
        assert_eq!(popup.cursor, 0);
        popup.handle_key(key(KeyCode::Up));
        assert_eq!(popup.cursor, 1); // wrapped to end
    }

    #[test]
    fn d_toggles_revoke_on_approved() {
        let mut popup = make_popup(vec![make_approved("discord", "456")]);
        popup.handle_key(key(KeyCode::Char('d')));
        match &popup.items[0] {
            PairingItem::Approved { pending_revoke, .. } => assert!(*pending_revoke),
            _ => panic!("Expected Approved item"),
        }
        // Toggle back
        popup.handle_key(key(KeyCode::Char('d')));
        match &popup.items[0] {
            PairingItem::Approved { pending_revoke, .. } => assert!(!*pending_revoke),
            _ => panic!("Expected Approved item"),
        }
    }

    #[test]
    fn d_ignored_on_pending() {
        let mut popup = make_popup(vec![make_pending("telegram", "123", "TG_ABC")]);
        let result = popup.handle_key(key(KeyCode::Char('d')));
        assert!(result.is_none());
    }

    #[test]
    fn d_ignored_on_unpaired() {
        let mut popup = make_popup(vec![make_unpaired("telegram")]);
        let result = popup.handle_key(key(KeyCode::Char('d')));
        assert!(result.is_none());
    }

    #[test]
    fn enter_approves_pending() {
        let mut popup = make_popup(vec![make_pending("telegram", "123", "TG_ABC")]);
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            PairingAction::Approve { channel, code } => {
                assert_eq!(channel, "telegram");
                assert_eq!(code, "TG_ABC");
            }
            _ => panic!("Expected Approve action"),
        }
        assert!(!popup.is_visible());
    }

    #[test]
    fn a_approves_pending() {
        let mut popup = make_popup(vec![make_pending("telegram", "123", "TG_ABC")]);
        let result = popup.handle_key(key(KeyCode::Char('a')));
        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            PairingAction::Approve { channel, code } => {
                assert_eq!(channel, "telegram");
                assert_eq!(code, "TG_ABC");
            }
            _ => panic!("Expected Approve action"),
        }
    }

    #[test]
    fn enter_collects_revokes() {
        let mut popup = make_popup(vec![
            make_approved("discord", "456"),
            make_approved("slack", "789"),
        ]);
        // Mark first for revoke
        popup.handle_key(key(KeyCode::Char('d')));
        // Move to second and mark too
        popup.handle_key(key(KeyCode::Down));
        popup.handle_key(key(KeyCode::Char('d')));
        // Now cursor is on an approved item, Enter collects revokes
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn enter_no_changes_shows_status() {
        let mut popup = make_popup(vec![make_approved("discord", "456")]);
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_none());
        assert!(popup.status_message.is_some());
        assert!(!popup.status_message.as_ref().unwrap().1); // not success
    }

    #[test]
    fn enter_on_unpaired_shows_status() {
        let mut popup = make_popup(vec![make_unpaired("telegram")]);
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_none());
        assert!(popup.is_visible()); // stays open
        assert!(popup.status_message.is_some());
        let msg = &popup.status_message.as_ref().unwrap().0;
        assert!(msg.contains("Telegram"));
    }

    #[test]
    fn hidden_popup_ignores_keys() {
        let mut popup = make_popup(vec![make_pending("telegram", "123", "TG_ABC")]);
        popup.visible = false;
        let result = popup.handle_key(key(KeyCode::Enter));
        assert!(result.is_none());
    }
}
