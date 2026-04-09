use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::db::ProjectRow;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

#[derive(Clone)]
enum Phase {
    Browsing,
    /// Creating a new project: step 1 = name, step 2 = description.
    Creating {
        step: CreateStep,
        name: String,
        buffer: String,
    },
    /// Editing an existing project field.
    Editing {
        field: EditField,
        buffer: String,
    },
}

#[derive(Clone, Copy)]
enum CreateStep {
    Name,
    Description,
}

#[derive(Clone, Copy)]
enum EditField {
    Name,
    Description,
}

pub struct ProjectsPopup {
    visible: bool,
    projects: Vec<ProjectItem>,
    cursor: usize,
    phase: Phase,
    status_message: Option<(String, bool)>,
}

struct ProjectItem {
    project: ProjectRow,
    workflow_count: usize,
    original_name: String,
    original_description: String,
    original_status: String,
    pending_delete: bool,
}

pub enum ProjectAction {
    Create {
        id: String,
        name: String,
        description: String,
    },
    Update {
        project_id: String,
        name: Option<String>,
        description: Option<String>,
    },
    Archive {
        project_id: String,
    },
    Unarchive {
        project_id: String,
    },
    Delete {
        project_id: String,
    },
}

impl ProjectsPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            projects: Vec::new(),
            cursor: 0,
            phase: Phase::Browsing,
            status_message: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.cursor = 0;
        self.phase = Phase::Browsing;
        self.status_message = None;

        self.projects = match borg_core::db::Database::open() {
            Ok(db) => match db.list_projects(None) {
                Ok(rows) => rows
                    .into_iter()
                    .map(|p| {
                        let wf_count = db
                            .list_workflows_by_project(&p.id)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        let original_name = p.name.clone();
                        let original_description = p.description.clone();
                        let original_status = p.status.clone();
                        ProjectItem {
                            project: p,
                            workflow_count: wf_count,
                            original_name,
                            original_description,
                            original_status,
                            pending_delete: false,
                        }
                    })
                    .collect(),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.phase = Phase::Browsing;
        self.status_message = None;
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.visible {
            return false;
        }
        match &mut self.phase {
            Phase::Creating { buffer, .. } | Phase::Editing { buffer, .. } => {
                buffer.push_str(text);
                true
            }
            Phase::Browsing => false,
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Vec<ProjectAction>> {
        use crossterm::event::KeyCode;

        if !self.visible {
            return None;
        }

        match &mut self.phase {
            Phase::Browsing => match key.code {
                KeyCode::Esc => {
                    self.dismiss();
                    None
                }
                KeyCode::Up => {
                    if !self.projects.is_empty() {
                        if self.cursor == 0 {
                            self.cursor = self.projects.len() - 1;
                        } else {
                            self.cursor -= 1;
                        }
                    }
                    self.status_message = None;
                    None
                }
                KeyCode::Down => {
                    if !self.projects.is_empty() {
                        self.cursor = (self.cursor + 1) % self.projects.len();
                    }
                    self.status_message = None;
                    None
                }
                KeyCode::Char('n') => {
                    self.phase = Phase::Creating {
                        step: CreateStep::Name,
                        name: String::new(),
                        buffer: String::new(),
                    };
                    self.status_message = None;
                    None
                }
                KeyCode::Char('e') => {
                    if let Some(item) = self.projects.get(self.cursor) {
                        self.phase = Phase::Editing {
                            field: EditField::Name,
                            buffer: item.project.name.clone(),
                        };
                        self.status_message = None;
                    }
                    None
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = self.projects.get_mut(self.cursor) {
                        match item.project.status.as_str() {
                            "active" => {
                                item.project.status = "archived".to_string();
                                self.status_message = Some(("Archived".to_string(), true));
                            }
                            "archived" => {
                                item.project.status = "active".to_string();
                                self.status_message = Some(("Unarchived".to_string(), true));
                            }
                            _ => {}
                        }
                    }
                    None
                }
                KeyCode::Char('d') => {
                    if let Some(item) = self.projects.get_mut(self.cursor) {
                        item.pending_delete = !item.pending_delete;
                        if item.pending_delete {
                            self.status_message = Some(("Will delete".to_string(), true));
                        } else {
                            self.status_message = None;
                        }
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
            Phase::Creating { step, name, buffer } => {
                // Clone to avoid borrow issues
                let current_step = *step;
                match key.code {
                    KeyCode::Esc => {
                        self.phase = Phase::Browsing;
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
                        let val = buffer.clone();
                        if val.trim().is_empty() {
                            self.status_message =
                                Some(("Field cannot be empty".to_string(), false));
                            return None;
                        }
                        match current_step {
                            CreateStep::Name => {
                                let saved_name = val;
                                self.phase = Phase::Creating {
                                    step: CreateStep::Description,
                                    name: saved_name,
                                    buffer: String::new(),
                                };
                                self.status_message = None;
                                None
                            }
                            CreateStep::Description => {
                                let id = uuid::Uuid::new_v4().to_string();
                                let action = ProjectAction::Create {
                                    id,
                                    name: name.clone(),
                                    description: val,
                                };
                                self.dismiss();
                                Some(vec![action])
                            }
                        }
                    }
                    _ => None,
                }
            }
            Phase::Editing { field, buffer } => {
                let current_field = *field;
                match key.code {
                    KeyCode::Esc => {
                        self.phase = Phase::Browsing;
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
                        let val = buffer.clone();
                        if val.trim().is_empty() {
                            self.status_message =
                                Some(("Field cannot be empty".to_string(), false));
                            return None;
                        }
                        match current_field {
                            EditField::Name => {
                                // Save name, move to editing description
                                if let Some(item) = self.projects.get(self.cursor) {
                                    let current_desc = item.project.description.clone();
                                    // Store the new name temporarily
                                    if let Some(item) = self.projects.get_mut(self.cursor) {
                                        item.project.name = val;
                                    }
                                    self.phase = Phase::Editing {
                                        field: EditField::Description,
                                        buffer: current_desc,
                                    };
                                    self.status_message = None;
                                }
                                None
                            }
                            EditField::Description => {
                                if let Some(item) = self.projects.get_mut(self.cursor) {
                                    item.project.description = val;
                                }
                                self.phase = Phase::Browsing;
                                self.status_message = Some(("Updated".to_string(), true));
                                None
                            }
                        }
                    }
                    _ => None,
                }
            }
        }
    }

    fn collect_actions(&self) -> Vec<ProjectAction> {
        let mut actions = Vec::new();

        for item in &self.projects {
            if item.pending_delete {
                actions.push(ProjectAction::Delete {
                    project_id: item.project.id.clone(),
                });
                continue;
            }

            // Check for status changes
            if item.project.status != item.original_status {
                if item.project.status == "archived" {
                    actions.push(ProjectAction::Archive {
                        project_id: item.project.id.clone(),
                    });
                } else {
                    actions.push(ProjectAction::Unarchive {
                        project_id: item.project.id.clone(),
                    });
                }
            }

            // Check for name/description edits
            let name_changed = item.project.name != item.original_name;
            let desc_changed = item.project.description != item.original_description;
            if name_changed || desc_changed {
                actions.push(ProjectAction::Update {
                    project_id: item.project.id.clone(),
                    name: if name_changed {
                        Some(item.project.name.clone())
                    } else {
                        None
                    },
                    description: if desc_changed {
                        Some(item.project.description.clone())
                    } else {
                        None
                    },
                });
            }
        }

        actions
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Projects", 5, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut row_indices: Vec<usize> = Vec::new();

        if self.projects.is_empty() && matches!(self.phase, Phase::Browsing) {
            lines.push(Line::from(Span::styled(
                " No projects yet".to_string(),
                theme::dim(),
            )));
        }

        for (i, item) in self.projects.iter().enumerate() {
            row_indices.push(lines.len());

            let status_icon = if item.project.status == "active" {
                "●"
            } else {
                "○"
            };

            let wf_label = if item.workflow_count == 1 {
                "1 workflow".to_string()
            } else {
                format!("{} workflows", item.workflow_count)
            };

            let label = format!("  {status_icon} {:<30} ({wf_label})", item.project.name,);

            let is_cursor = i == self.cursor;
            let style = if is_cursor {
                theme::popup_selected()
            } else if item.project.status == "archived" {
                theme::dim()
            } else {
                ratatui::style::Style::default()
            };

            lines.push(Line::from(Span::styled(label, style)));

            // Description line
            if !item.project.description.is_empty() {
                let desc: String = item.project.description.chars().take(50).collect();
                let detail_style = if is_cursor {
                    theme::popup_selected()
                } else {
                    theme::dim()
                };
                lines.push(Line::from(Span::styled(
                    format!("      {desc}"),
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

        // Render input overlay for Creating/Editing phases
        match &self.phase {
            Phase::Creating { step, buffer, .. } => {
                let label = match step {
                    CreateStep::Name => "Project name:",
                    CreateStep::Description => "Description:",
                };
                self.render_input_overlay(frame, inner, label, buffer);
            }
            Phase::Editing { field, buffer } => {
                let label = match field {
                    EditField::Name => "Edit name:",
                    EditField::Description => "Edit description:",
                };
                self.render_input_overlay(frame, inner, label, buffer);
            }
            Phase::Browsing => {}
        }

        let hint = match &self.phase {
            Phase::Browsing => {
                " n: new  e: edit  Space: archive  d: delete  Enter: apply  Esc: close"
            }
            Phase::Creating { .. } | Phase::Editing { .. } => " Enter: confirm  Esc: cancel",
        };
        popup_utils::render_footer(frame, inner, hint);
    }

    fn render_input_overlay(&self, frame: &mut Frame, inner: Rect, label: &str, buffer: &str) {
        let edit_y = inner.y + inner.height.saturating_sub(4);
        let edit_area = Rect::new(inner.x, edit_y, inner.width, 2);
        frame.render_widget(Clear, edit_area);
        let edit_lines = vec![
            Line::from(Span::styled(format!(" {label}"), theme::bold())),
            Line::from(Span::styled(
                format!("   > {buffer}_"),
                theme::popup_selected(),
            )),
        ];
        frame.render_widget(Paragraph::new(edit_lines), edit_area);
    }
}

impl PopupHandler for ProjectsPopup {
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
            .map(|actions| AppAction::RunProjectActions { actions }))
    }

    fn handle_paste_event(&mut self, text: &str) -> bool {
        self.handle_paste(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_popup_with_project(status: &str) -> ProjectsPopup {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        popup.projects.push(ProjectItem {
            project: ProjectRow {
                id: "proj-1".into(),
                name: "Test Project".into(),
                description: "A test".into(),
                status: status.into(),
                created_at: 1000,
                updated_at: 1000,
            },
            workflow_count: 2,
            original_name: "Test Project".into(),
            original_description: "A test".into(),
            original_status: status.into(),
            pending_delete: false,
        });
        popup
    }

    #[test]
    fn new_popup_not_visible() {
        let popup = ProjectsPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn dismiss_hides_popup() {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!popup.is_visible());
    }

    #[test]
    fn space_toggles_archive() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);

        popup.handle_key(space);
        assert_eq!(popup.projects[0].project.status, "archived");

        popup.handle_key(space);
        assert_eq!(popup.projects[0].project.status, "active");
    }

    #[test]
    fn d_marks_for_delete() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);

        popup.handle_key(d);
        assert!(popup.projects[0].pending_delete);

        popup.handle_key(d);
        assert!(!popup.projects[0].pending_delete);
    }

    #[test]
    fn archive_collected_in_actions() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ProjectAction::Archive { .. }));
    }

    #[test]
    fn unarchive_collected_in_actions() {
        let mut popup = make_popup_with_project("archived");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ProjectAction::Unarchive { .. }));
    }

    #[test]
    fn delete_collected_in_actions() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ProjectAction::Delete { .. }));
    }

    #[test]
    fn no_changes_enter_returns_none() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(result.is_none());
    }

    #[test]
    fn n_enters_create_mode() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(popup.phase, Phase::Creating { .. }));
    }

    #[test]
    fn create_flow_name_then_description() {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // Start create
        popup.handle_key(key('n'));

        // Type name
        popup.handle_key(key('M'));
        popup.handle_key(key('y'));
        popup.handle_key(enter); // confirm name → move to description

        assert!(matches!(
            popup.phase,
            Phase::Creating {
                step: CreateStep::Description,
                ..
            }
        ));

        // Type description
        popup.handle_key(key('D'));
        popup.handle_key(key('s'));
        let result = popup.handle_key(enter); // confirm description → create

        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ProjectAction::Create {
                name, description, ..
            } => {
                assert_eq!(name, "My");
                assert_eq!(description, "Ds");
            }
            _ => panic!("expected Create action"),
        }
    }

    #[test]
    fn e_enters_edit_mode() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        match &popup.phase {
            Phase::Editing { field, buffer } => {
                assert!(matches!(field, EditField::Name));
                assert_eq!(buffer, "Test Project");
            }
            _ => panic!("expected Editing phase"),
        }
    }

    #[test]
    fn edit_flow_name_then_description() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        // Enter edit mode
        popup.handle_key(key('e'));

        // Clear and type new name
        for _ in 0..20 {
            popup.handle_key(backspace);
        }
        popup.handle_key(key('N'));
        popup.handle_key(key('e'));
        popup.handle_key(key('w'));
        popup.handle_key(enter); // confirm name → description

        assert!(matches!(
            popup.phase,
            Phase::Editing {
                field: EditField::Description,
                ..
            }
        ));
        // Name should be updated on the item
        assert_eq!(popup.projects[0].project.name, "New");

        // Confirm description as-is
        popup.handle_key(enter);
        assert!(matches!(popup.phase, Phase::Browsing));
    }

    #[test]
    fn esc_cancels_create() {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(popup.phase, Phase::Creating { .. }));

        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(popup.phase, Phase::Browsing));
        assert!(popup.is_visible()); // popup stays open
    }

    #[test]
    fn empty_name_rejected() {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        popup.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        // Try to submit empty name
        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(result.is_none());
        assert!(popup.status_message.as_ref().unwrap().0.contains("empty"));
    }

    #[test]
    fn paste_consumed_in_create_mode() {
        let mut popup = ProjectsPopup::new();
        popup.visible = true;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert!(!popup.handle_paste("test")); // not consumed in browsing

        popup.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(popup.handle_paste("pasted name")); // consumed in creating
    }

    #[test]
    fn cursor_wraps() {
        let mut popup = make_popup_with_project("active");
        popup.projects.push(ProjectItem {
            project: ProjectRow {
                id: "proj-2".into(),
                name: "Second".into(),
                description: "".into(),
                status: "active".into(),
                created_at: 1000,
                updated_at: 1000,
            },
            workflow_count: 0,
            original_name: "Second".into(),
            original_description: "".into(),
            original_status: "active".into(),
            pending_delete: false,
        });
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);

        assert_eq!(popup.cursor, 0);
        popup.handle_key(down);
        assert_eq!(popup.cursor, 1);
        popup.handle_key(down);
        assert_eq!(popup.cursor, 0); // wrapped

        popup.handle_key(up);
        assert_eq!(popup.cursor, 1); // wrapped up
    }

    #[test]
    fn edit_produces_update_action() {
        let mut popup = make_popup_with_project("active");
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        // Enter edit, change name
        popup.handle_key(key('e'));
        for _ in 0..20 {
            popup.handle_key(backspace);
        }
        popup.handle_key(key('X'));
        popup.handle_key(enter); // confirm name → description
        popup.handle_key(enter); // confirm description unchanged

        // Now apply
        let result = popup.handle_key(enter);
        assert!(result.is_some());
        let actions = result.unwrap();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ProjectAction::Update {
                name, description, ..
            } => {
                assert_eq!(name.as_deref(), Some("X"));
                assert!(description.is_none()); // unchanged
            }
            _ => panic!("expected Update action"),
        }
    }
}
