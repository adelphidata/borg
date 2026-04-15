use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::db::ScheduledTaskRow;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

#[derive(Clone)]
enum SchedulePhase {
    Browsing,
    EditingSchedule { buffer: String },
}

pub struct SchedulePopup {
    visible: bool,
    tasks: Vec<TaskItem>,
    cursor: usize,
    phase: SchedulePhase,
    status_message: Option<(String, bool)>,
}

struct TaskItem {
    task: ScheduledTaskRow,
    original_status: String,
    original_schedule_expr: String,
    pending_delete: bool,
}

pub enum ScheduleAction {
    ToggleStatus {
        task_id: String,
        new_status: String,
    },
    UpdateSchedule {
        task_id: String,
        schedule_type: String,
        new_expr: String,
    },
    DeleteTask {
        task_id: String,
    },
}

impl SchedulePopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            tasks: Vec::new(),
            cursor: 0,
            phase: SchedulePhase::Browsing,
            status_message: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.cursor = 0;
        self.phase = SchedulePhase::Browsing;
        self.status_message = None;

        self.tasks = match borg_core::db::Database::open().and_then(|db| db.list_tasks()) {
            Ok(rows) => rows
                .into_iter()
                .map(|task| {
                    let original_status = task.status.clone();
                    let original_schedule_expr = task.schedule_expr.clone();
                    TaskItem {
                        task,
                        original_status,
                        original_schedule_expr,
                        pending_delete: false,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.phase = SchedulePhase::Browsing;
        self.status_message = None;
    }

    /// Handle a bracketed paste event. Returns `true` if the paste was consumed
    /// (i.e. the popup is visible and in schedule-editing phase).
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.visible {
            return false;
        }
        if let SchedulePhase::EditingSchedule { ref mut buffer } = self.phase {
            buffer.push_str(text);
            return true;
        }
        false
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Vec<ScheduleAction>> {
        use crossterm::event::KeyCode;

        if !self.visible {
            return None;
        }

        match &mut self.phase {
            SchedulePhase::Browsing => match key.code {
                KeyCode::Esc => {
                    self.dismiss();
                    None
                }
                KeyCode::Up => {
                    let total = self.tasks.len();
                    if total > 0 {
                        if self.cursor == 0 {
                            self.cursor = total - 1;
                        } else {
                            self.cursor -= 1;
                        }
                    }
                    self.status_message = None;
                    None
                }
                KeyCode::Down => {
                    let total = self.tasks.len();
                    if total > 0 {
                        self.cursor = (self.cursor + 1) % total;
                    }
                    self.status_message = None;
                    None
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = self.tasks.get_mut(self.cursor) {
                        let status = item.task.status.as_str();
                        match status {
                            "active" => {
                                item.task.status = "paused".to_string();
                                self.status_message = Some(("Paused".to_string(), true));
                            }
                            "paused" => {
                                item.task.status = "active".to_string();
                                self.status_message = Some(("Resumed".to_string(), true));
                            }
                            _ => {
                                self.status_message =
                                    Some((format!("Cannot toggle {status} task"), false));
                            }
                        }
                    }
                    None
                }
                KeyCode::Char('e') => {
                    if let Some(item) = self.tasks.get(self.cursor) {
                        if item.task.status == "cancelled" || item.task.status == "completed" {
                            self.status_message =
                                Some(("Cannot edit a finished task".to_string(), false));
                            return None;
                        }
                        let buf = item.task.schedule_expr.clone();
                        self.phase = SchedulePhase::EditingSchedule { buffer: buf };
                    }
                    None
                }
                KeyCode::Char('d') => {
                    if let Some(item) = self.tasks.get_mut(self.cursor) {
                        item.pending_delete = !item.pending_delete;
                    }
                    None
                }
                KeyCode::Enter => {
                    let actions = self.collect_actions();
                    if actions.is_empty() {
                        self.status_message = Some(("No changes".to_string(), false));
                        return None;
                    }
                    self.dismiss();
                    Some(actions)
                }
                _ => None,
            },
            SchedulePhase::EditingSchedule { ref mut buffer } => match key.code {
                KeyCode::Esc => {
                    self.phase = SchedulePhase::Browsing;
                    self.status_message = None;
                    None
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    None
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    None
                }
                KeyCode::Enter => {
                    let buf_snapshot = buffer.clone();
                    if buf_snapshot.is_empty() {
                        self.status_message =
                            Some(("Expression cannot be empty".to_string(), false));
                        return None;
                    }
                    if let Some(item) = self.tasks.get(self.cursor) {
                        let stype = &item.task.schedule_type;
                        if let Err(e) = borg_core::tasks::validate_schedule(stype, &buf_snapshot) {
                            self.status_message = Some((format!("Invalid: {e}"), false));
                            return None;
                        }
                    }
                    if let Some(item) = self.tasks.get_mut(self.cursor) {
                        item.task.schedule_expr = buf_snapshot;
                    }
                    self.phase = SchedulePhase::Browsing;
                    self.status_message = Some(("Schedule updated".to_string(), true));
                    None
                }
                _ => None,
            },
        }
    }

    fn collect_actions(&self) -> Vec<ScheduleAction> {
        let mut actions = Vec::new();

        for item in &self.tasks {
            if item.pending_delete {
                actions.push(ScheduleAction::DeleteTask {
                    task_id: item.task.id.clone(),
                });
                continue;
            }

            if item.task.status != item.original_status {
                actions.push(ScheduleAction::ToggleStatus {
                    task_id: item.task.id.clone(),
                    new_status: item.task.status.clone(),
                });
            }
            if item.task.schedule_expr != item.original_schedule_expr {
                actions.push(ScheduleAction::UpdateSchedule {
                    task_id: item.task.id.clone(),
                    schedule_type: item.task.schedule_type.clone(),
                    new_expr: item.task.schedule_expr.clone(),
                });
            }
        }

        actions
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Schedule", 5, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut row_indices: Vec<usize> = Vec::new();

        if self.tasks.is_empty() {
            lines.push(Line::from(Span::styled(
                " Nothing scheduled".to_string(),
                theme::dim(),
            )));
        }

        for (i, item) in self.tasks.iter().enumerate() {
            row_indices.push(lines.len());

            let check = match item.task.status.as_str() {
                "active" => "x",
                "paused" => " ",
                _ => "-",
            };

            let next_str = item
                .task
                .next_run
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|dt| format!("Next: {}", dt.format("%Y-%m-%d %H:%M UTC")))
                .unwrap_or_default();

            let label = format!(
                "  [{check}] {:<28} {} {}",
                item.task.name, item.task.schedule_type, item.task.schedule_expr,
            );

            let is_cursor = self.cursor == i;
            let style = if is_cursor {
                theme::popup_selected()
            } else if item.task.status == "cancelled" || item.task.status == "completed" {
                theme::dim()
            } else {
                ratatui::style::Style::default()
            };

            lines.push(Line::from(Span::styled(label, style)));

            if !next_str.is_empty() {
                let detail_style = if is_cursor {
                    theme::popup_selected()
                } else {
                    theme::dim()
                };
                lines.push(Line::from(Span::styled(
                    format!("      {next_str}"),
                    detail_style,
                )));
            }
        }

        let selected_line = row_indices.get(self.cursor).copied().unwrap_or(0);
        let scroll_offset = if selected_line >= content_height {
            selected_line - content_height + 1
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

        if let SchedulePhase::EditingSchedule { ref buffer } = self.phase {
            let edit_y = inner.y + inner.height.saturating_sub(4);
            let edit_area = Rect::new(inner.x, edit_y, inner.width, 2);
            frame.render_widget(Clear, edit_area);
            let edit_lines = vec![
                Line::from(Span::styled(
                    " Edit schedule expression:".to_string(),
                    theme::bold(),
                )),
                Line::from(Span::styled(
                    format!("   > {buffer}_"),
                    theme::popup_selected(),
                )),
            ];
            frame.render_widget(Paragraph::new(edit_lines), edit_area);
        }

        let hint = if matches!(self.phase, SchedulePhase::EditingSchedule { .. }) {
            " Enter: save  Esc: cancel"
        } else {
            " Space: toggle  e: edit  d: delete  Enter: apply  Esc: close"
        };
        popup_utils::render_footer(frame, inner, hint);
    }
}

impl PopupHandler for SchedulePopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        Ok(self
            .handle_key(key)
            .map(|actions| AppAction::RunScheduleActions { actions }))
    }

    fn handle_paste_event(&mut self, text: &str) -> bool {
        self.handle_paste(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_popup_not_visible() {
        let popup = SchedulePopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = SchedulePopup::new();
        popup.show();
        assert!(popup.is_visible());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses() {
        let mut popup = SchedulePopup::new();
        popup.show();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        popup.handle_key(esc);
        assert!(!popup.is_visible());
    }

    #[test]
    fn no_changes_enter_returns_none() {
        let mut popup = SchedulePopup::new();
        popup.show();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = popup.handle_key(enter);
        assert!(result.is_none());
    }

    fn make_popup_with_task(status: &str) -> SchedulePopup {
        let mut popup = SchedulePopup::new();
        popup.visible = true;
        popup.tasks.push(TaskItem {
            task: ScheduledTaskRow {
                id: "test-1".into(),
                name: "test task".into(),
                prompt: "do something".into(),
                schedule_type: "cron".into(),
                schedule_expr: "0 9 * * *".into(),
                timezone: "UTC".into(),
                status: status.into(),
                next_run: None,
                created_at: 0,
                max_retries: 0,
                retry_count: 0,
                retry_after: None,
                last_error: None,
                timeout_ms: 60000,
                delivery_channel: None,
                delivery_target: None,
                allowed_tools: None,
                task_type: "prompt".into(),
            },
            original_status: status.into(),
            original_schedule_expr: "0 9 * * *".into(),
            pending_delete: false,
        });
        popup
    }

    #[test]
    fn space_toggles_task_status() {
        let mut popup = make_popup_with_task("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);

        let result = popup.handle_key(space);
        assert_eq!(popup.tasks[0].task.status, "paused");
        assert!(result.is_none());

        let result = popup.handle_key(space);
        assert_eq!(popup.tasks[0].task.status, "active");
        assert!(result.is_none());
    }

    #[test]
    fn enter_does_not_toggle_triggers_apply() {
        let mut popup = make_popup_with_task("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        let result = popup.handle_key(enter);
        assert_eq!(popup.tasks[0].task.status, "active");
        assert!(result.is_none());

        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        popup.handle_key(space);
        assert_eq!(popup.tasks[0].task.status, "paused");

        let result = popup.handle_key(enter);
        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ScheduleAction::ToggleStatus { .. }));
    }

    #[test]
    fn tab_does_nothing() {
        let mut popup = make_popup_with_task("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

        let result = popup.handle_key(tab);
        assert_eq!(popup.tasks[0].task.status, "active");
        assert!(result.is_none());
        assert!(popup.status_message.is_none());
    }

    #[test]
    fn handle_paste_consumed_in_editing_schedule() {
        let mut popup = make_popup_with_task("active");

        assert!(!popup.handle_paste("anything"));

        popup.phase = SchedulePhase::EditingSchedule {
            buffer: String::new(),
        };
        assert!(popup.handle_paste("*/5 * * * *"));
        if let SchedulePhase::EditingSchedule { ref buffer } = popup.phase {
            assert_eq!(buffer, "*/5 * * * *");
        } else {
            panic!("expected EditingSchedule phase");
        }
    }

    #[test]
    fn handle_paste_not_consumed_when_hidden() {
        let popup = &mut SchedulePopup::new();
        assert!(!popup.handle_paste("anything"));
    }
}
