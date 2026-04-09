use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::migrate::{self, MigrationCategories, MigrationPlan, MigrationSource, SourceData};

use super::app::{AppAction, PopupHandler};
use super::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigratePhase {
    SourceSelection,
    Categories,
    Preview,
    Confirm,
}

pub enum MigrateAction {
    Apply {
        plan: MigrationPlan,
        source_data: SourceData,
    },
}

pub struct MigratePopup {
    visible: bool,
    phase: MigratePhase,

    // Source selection
    available_sources: Vec<MigrationSource>,
    source_cursor: usize,

    // Categories
    categories: MigrationCategories,
    category_cursor: usize,

    // Preview
    plan: Option<MigrationPlan>,
    source_data: Option<SourceData>,
    preview_lines: Vec<String>,
    preview_scroll: usize,

    // Status
    error: Option<String>,
}

impl MigratePopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            phase: MigratePhase::SourceSelection,
            available_sources: Vec::new(),
            source_cursor: 0,
            categories: MigrationCategories::default(),
            category_cursor: 0,
            plan: None,
            source_data: None,
            preview_lines: Vec::new(),
            preview_scroll: 0,
            error: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.phase = MigratePhase::SourceSelection;
        self.available_sources = migrate::detect_sources();
        self.source_cursor = 0;
        self.categories = MigrationCategories::default();
        self.category_cursor = 0;
        self.plan = None;
        self.source_data = None;
        self.preview_lines.clear();
        self.preview_scroll = 0;
        self.error = None;
    }

    fn dismiss(&mut self) {
        self.visible = false;
    }

    fn selected_source(&self) -> Option<MigrationSource> {
        self.available_sources.get(self.source_cursor).copied()
    }

    fn can_advance(&self) -> bool {
        match self.phase {
            MigratePhase::SourceSelection => !self.available_sources.is_empty(),
            MigratePhase::Categories | MigratePhase::Preview => true,
            MigratePhase::Confirm => false,
        }
    }

    fn advance(&mut self) {
        if !self.can_advance() {
            return;
        }
        match self.phase {
            MigratePhase::SourceSelection => self.phase = MigratePhase::Categories,
            MigratePhase::Categories => {
                self.plan = None;
                self.source_data = None;
                self.build_preview();
                self.phase = MigratePhase::Preview;
            }
            MigratePhase::Preview => self.phase = MigratePhase::Confirm,
            MigratePhase::Confirm => {}
        }
    }

    fn go_back(&mut self) {
        match self.phase {
            MigratePhase::SourceSelection => self.dismiss(),
            MigratePhase::Categories => self.phase = MigratePhase::SourceSelection,
            MigratePhase::Preview => self.phase = MigratePhase::Categories,
            MigratePhase::Confirm => self.phase = MigratePhase::Preview,
        }
    }

    fn build_preview(&mut self) {
        let Some(source) = self.selected_source() else {
            return;
        };

        match migrate::parse_source(source, &self.categories) {
            Ok(data) => {
                let borg_dir = Config::data_dir().unwrap_or_default();
                let config = Config::load_from_db().unwrap_or_default();
                let plan = migrate::plan::build_plan(source, &data, &config, &borg_dir);
                self.preview_lines = plan.summary_lines();
                self.source_data = Some(data);
                self.plan = Some(plan);
                self.preview_scroll = 0;
            }
            Err(e) => {
                self.preview_lines = vec![format!("Error parsing source: {e}")];
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Vec<MigrateAction>> {
        if key.code == KeyCode::Esc {
            self.go_back();
            return None;
        }

        match self.phase {
            MigratePhase::SourceSelection => self.handle_source_key(key),
            MigratePhase::Categories => self.handle_categories_key(key),
            MigratePhase::Preview => self.handle_preview_key(key),
            MigratePhase::Confirm => self.handle_confirm_key(key),
        }
    }

    fn handle_source_key(&mut self, key: KeyEvent) -> Option<Vec<MigrateAction>> {
        match key.code {
            KeyCode::Up => {
                if self.source_cursor > 0 {
                    self.source_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.source_cursor + 1 < self.available_sources.len() {
                    self.source_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Tab => {
                if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.go_back();
                } else {
                    self.advance();
                }
            }
            _ => {}
        }
        None
    }

    fn handle_categories_key(&mut self, key: KeyEvent) -> Option<Vec<MigrateAction>> {
        match key.code {
            KeyCode::Up => {
                if self.category_cursor > 0 {
                    self.category_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.category_cursor < MigrationCategories::LABELS.len() - 1 {
                    self.category_cursor += 1;
                }
            }
            KeyCode::Char(' ') => {
                self.categories.toggle(self.category_cursor);
            }
            KeyCode::Enter | KeyCode::Tab => {
                if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.go_back();
                } else {
                    self.advance();
                }
            }
            _ => {}
        }
        None
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> Option<Vec<MigrateAction>> {
        match key.code {
            KeyCode::Up => {
                if self.preview_scroll > 0 {
                    self.preview_scroll -= 1;
                }
            }
            KeyCode::Down => {
                if self.preview_scroll + 1 < self.preview_lines.len() {
                    self.preview_scroll += 1;
                }
            }
            KeyCode::Enter | KeyCode::Tab => {
                if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.go_back();
                } else {
                    self.advance();
                }
            }
            _ => {}
        }
        None
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<Vec<MigrateAction>> {
        match key.code {
            KeyCode::Enter => {
                if let (Some(plan), Some(data)) = (self.plan.take(), self.source_data.take()) {
                    self.dismiss();
                    return Some(vec![MigrateAction::Apply {
                        plan,
                        source_data: data,
                    }]);
                }
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.go_back();
            }
            _ => {}
        }
        None
    }

    pub fn render(&self, frame: &mut Frame) {
        if !self.visible {
            return;
        }

        let area = frame.area();
        let popup_width = (area.width * 60 / 100)
            .max(44)
            .min(area.width.saturating_sub(4));
        let popup_height = (area.height * 70 / 100)
            .max(16)
            .min(area.height.saturating_sub(4));

        let x = (area.width.saturating_sub(popup_width)) / 2;
        let y = (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_height);

        frame.render_widget(Clear, popup_area);

        let phase_label = match self.phase {
            MigratePhase::SourceSelection => "Source",
            MigratePhase::Categories => "Categories",
            MigratePhase::Preview => "Preview",
            MigratePhase::Confirm => "Confirm",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(format!(" Migrate > {phase_label} "));
        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

        match self.phase {
            MigratePhase::SourceSelection => self.render_source(frame, layout[0]),
            MigratePhase::Categories => self.render_categories(frame, layout[0]),
            MigratePhase::Preview => self.render_preview(frame, layout[0]),
            MigratePhase::Confirm => self.render_confirm(frame, layout[0]),
        }

        self.render_footer(frame, layout[1]);
    }

    fn render_source(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Choose a source:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        if self.available_sources.is_empty() {
            lines.push(Line::from(Span::styled(
                " No migration sources detected.",
                Style::default().fg(theme::RED),
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                " Expected directories:",
                theme::dim(),
            )));
            lines.push(Line::from(Span::styled(
                "   ~/.hermes/  (Hermes Agent)",
                theme::dim(),
            )));
            lines.push(Line::from(Span::styled(
                "   ~/.openclaw/  (OpenClaw)",
                theme::dim(),
            )));
        } else {
            for (i, source) in self.available_sources.iter().enumerate() {
                let selected = i == self.source_cursor;
                let marker = if selected { ">" } else { " " };
                let style = if selected {
                    Style::default().fg(theme::CYAN)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!(" {marker} {}", source.label()),
                    style,
                )));

                let dir_display = source.data_dir().display().to_string();
                lines.push(Line::from(Span::styled(
                    format!("     {dir_display}"),
                    theme::dim(),
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_categories(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " What to migrate:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        for (i, label) in MigrationCategories::LABELS.iter().enumerate() {
            let checked = self.categories.get(i);
            let focused = i == self.category_cursor;
            let checkbox = if checked { "[x]" } else { "[ ]" };
            let marker = if focused { ">" } else { " " };
            let style = if focused {
                Style::default().fg(theme::CYAN)
            } else {
                Style::default()
            };

            lines.push(Line::from(Span::styled(
                format!(" {marker} {checkbox} {label}"),
                style,
            )));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_preview(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::default());

        let source_label = self
            .selected_source()
            .map(|s| s.label())
            .unwrap_or("Unknown");
        lines.push(Line::from(Span::styled(
            format!(" Preview ({source_label}):"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        if let Some(ref err) = self.error {
            lines.push(Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme::RED),
            )));
        } else {
            let visible_height = area.height.saturating_sub(4) as usize;
            let end = (self.preview_scroll + visible_height).min(self.preview_lines.len());
            let start = self.preview_scroll.min(end);

            for line in &self.preview_lines[start..end] {
                let style = if line.contains("(skip:") {
                    theme::dim()
                } else if line.ends_with(':') {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!(" {line}"), style)));
            }

            if self.preview_lines.len() > visible_height {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!(
                        " [{}/{}] Use Up/Down to scroll",
                        self.preview_scroll + 1,
                        self.preview_lines.len()
                    ),
                    theme::dim(),
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::default());

        if let Some(ref err) = self.error {
            lines.push(Line::from(Span::styled(
                format!(" {err}"),
                Style::default().fg(theme::RED),
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                " Press Esc to go back.",
                theme::dim(),
            )));
        } else if let Some(ref plan) = self.plan {
            let active = plan.active_change_count();
            let creds = plan.credentials_to_add.len();
            let files = plan.memory_files.len() + if plan.persona_file.is_some() { 1 } else { 0 };
            let skills = plan.skill_dirs.len();

            lines.push(Line::from(Span::styled(
                " Ready to apply:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());

            if active > 0 {
                lines.push(Line::from(format!("   {active} settings")));
            }
            if creds > 0 {
                lines.push(Line::from(format!("   {creds} credentials")));
            }
            if files > 0 {
                lines.push(Line::from(format!("   {files} files")));
            }
            if skills > 0 {
                lines.push(Line::from(format!("   {skills} skills")));
            }

            if plan.is_empty() {
                lines.push(Line::from(Span::styled(
                    "   Nothing to migrate.",
                    theme::dim(),
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let hint = match self.phase {
            MigratePhase::SourceSelection if self.available_sources.is_empty() => "Esc: close",
            MigratePhase::SourceSelection => "Up/Down: select  Enter: next  Esc: close",
            MigratePhase::Categories => "Up/Down: move  Space: toggle  Enter: next  Esc: back",
            MigratePhase::Preview => "Up/Down: scroll  Enter: next  Esc: back",
            MigratePhase::Confirm => "Enter: apply  Esc: back",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {hint}"), theme::dim())))
                .alignment(Alignment::Left),
            area,
        );
    }
}

impl PopupHandler for MigratePopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> anyhow::Result<Option<AppAction>> {
        Ok(self
            .handle_key(key)
            .map(|actions| AppAction::RunMigration { actions }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn new_popup_not_visible() {
        let popup = MigratePopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn show_and_dismiss() {
        let mut popup = MigratePopup::new();
        popup.show();
        assert!(popup.is_visible());
        assert_eq!(popup.phase, MigratePhase::SourceSelection);

        // Esc from source selection dismisses
        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!popup.is_visible());
    }

    #[test]
    fn source_selection_navigation() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::SourceSelection;
        popup.available_sources = vec![MigrationSource::Hermes, MigrationSource::OpenClaw];
        popup.source_cursor = 0;

        // Down
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(popup.source_cursor, 1);

        // Can't go past end
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(popup.source_cursor, 1);

        // Up
        popup.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(popup.source_cursor, 0);

        // Can't go below 0
        popup.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(popup.source_cursor, 0);
    }

    #[test]
    fn source_selection_empty_blocks_advance() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::SourceSelection;
        popup.available_sources = Vec::new();

        popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(popup.phase, MigratePhase::SourceSelection);
    }

    #[test]
    fn esc_from_source_dismisses() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::SourceSelection;

        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_from_categories_goes_back() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Categories;

        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(popup.is_visible());
        assert_eq!(popup.phase, MigratePhase::SourceSelection);
    }

    #[test]
    fn esc_from_preview_goes_back() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Preview;

        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(popup.is_visible());
        assert_eq!(popup.phase, MigratePhase::Categories);
    }

    #[test]
    fn esc_from_confirm_goes_back() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Confirm;

        popup.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(popup.is_visible());
        assert_eq!(popup.phase, MigratePhase::Preview);
    }

    #[test]
    fn categories_toggle() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Categories;

        assert!(popup.categories.get(0)); // config starts true
        popup.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(!popup.categories.get(0)); // toggled off
        popup.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(popup.categories.get(0)); // toggled back on
    }

    #[test]
    fn categories_navigation() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Categories;
        assert_eq!(popup.category_cursor, 0);

        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(popup.category_cursor, 1);

        // Navigate to last
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(popup.category_cursor, 4);

        // Can't go past end
        popup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(popup.category_cursor, 4);

        popup.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(popup.category_cursor, 3);
    }

    #[test]
    fn phase_advance_from_source_to_categories() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::SourceSelection;
        popup.available_sources = vec![MigrationSource::Hermes];

        popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(popup.phase, MigratePhase::Categories);
    }

    #[test]
    fn confirm_with_plan_returns_action() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Confirm;
        popup.plan = Some(MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![],
        });
        popup.source_data = Some(SourceData::default());

        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(result.is_some());
        assert!(!popup.is_visible()); // dismissed after apply
    }

    #[test]
    fn confirm_without_plan_returns_none() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Confirm;
        popup.plan = None;
        popup.source_data = None;

        let result = popup.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(result.is_none());
    }

    #[test]
    fn shift_tab_goes_back() {
        let mut popup = MigratePopup::new();
        popup.visible = true;
        popup.phase = MigratePhase::Categories;

        popup.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(popup.phase, MigratePhase::SourceSelection);
    }
}
