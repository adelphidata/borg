use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::logo::LOGO;
use crate::tui::theme;
use borg_core::config::Config;
use borg_core::migrate::{
    self, MigrationCategories, MigrationPlan, MigrationResult, MigrationSource, SourceData,
};

const LOGO_HEIGHT: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Source,
    Categories,
    Preview,
    Confirm,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Source, Tab::Categories, Tab::Preview, Tab::Confirm];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Source => "Source",
            Tab::Categories => "Categories",
            Tab::Preview => "Preview",
            Tab::Confirm => "Confirm",
        }
    }
}

struct MigrateState {
    tab: Tab,

    // Source
    available_sources: Vec<MigrationSource>,
    selected_source: usize,

    // Categories
    categories: MigrationCategories,
    category_focus: usize,

    // Preview
    plan: Option<MigrationPlan>,
    source_data: Option<SourceData>,
    preview_scroll: usize,
    preview_lines: Vec<String>,

    // Result
    done: bool,
    cancelled: bool,
    result: Option<MigrationResult>,
    error: Option<String>,
}

impl MigrateState {
    fn new() -> Self {
        let available_sources = migrate::detect_sources();
        Self {
            tab: Tab::Source,
            available_sources,
            selected_source: 0,
            categories: MigrationCategories::default(),
            category_focus: 0,
            plan: None,
            source_data: None,
            preview_scroll: 0,
            preview_lines: Vec::new(),
            done: false,
            cancelled: false,
            result: None,
            error: None,
        }
    }

    #[cfg(test)]
    fn new_with_sources(sources: Vec<MigrationSource>) -> Self {
        Self {
            available_sources: sources,
            ..Self::new_empty()
        }
    }

    #[cfg(test)]
    fn new_empty() -> Self {
        Self {
            tab: Tab::Source,
            available_sources: Vec::new(),
            selected_source: 0,
            categories: MigrationCategories::default(),
            category_focus: 0,
            plan: None,
            source_data: None,
            preview_scroll: 0,
            preview_lines: Vec::new(),
            done: false,
            cancelled: false,
            result: None,
            error: None,
        }
    }

    fn selected_source(&self) -> Option<MigrationSource> {
        self.available_sources.get(self.selected_source).copied()
    }

    fn tab_completed(&self, tab: Tab) -> bool {
        match tab {
            Tab::Source => !self.available_sources.is_empty(),
            Tab::Categories => true,
            Tab::Preview => self.plan.is_some(),
            Tab::Confirm => false,
        }
    }

    fn can_advance(&self) -> bool {
        match self.tab {
            Tab::Source => !self.available_sources.is_empty(),
            Tab::Categories => true,
            Tab::Preview => true,
            Tab::Confirm => false,
        }
    }

    fn next_tab(&mut self) {
        if !self.can_advance() {
            return;
        }

        let idx = self.tab.index();
        if idx < Tab::ALL.len() - 1 {
            self.tab = Tab::ALL[idx + 1];

            // Build plan when entering Preview
            if self.tab == Tab::Preview && self.plan.is_none() {
                self.build_preview();
            }
        }
    }

    fn prev_tab(&mut self) {
        let idx = self.tab.index();
        if idx > 0 {
            self.tab = Tab::ALL[idx - 1];
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

    fn apply_migration(&mut self) {
        let Some(plan) = &self.plan else { return };
        let Some(data) = &self.source_data else {
            return;
        };

        let borg_dir = Config::data_dir().unwrap_or_default();
        match migrate::apply::apply_plan(plan, data, &borg_dir) {
            Ok(result) => {
                self.result = Some(result);
                self.done = true;
            }
            Err(e) => {
                self.error = Some(format!("Migration failed: {e}"));
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancelled = true;
            return;
        }

        match self.tab {
            Tab::Source => self.handle_source_key(key),
            Tab::Categories => self.handle_categories_key(key),
            Tab::Preview => self.handle_preview_key(key),
            Tab::Confirm => self.handle_confirm_key(key),
        }
    }

    fn handle_source_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.selected_source > 0 {
                    self.selected_source -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected_source + 1 < self.available_sources.len() {
                    self.selected_source += 1;
                }
            }
            KeyCode::Enter | KeyCode::Tab => {
                if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_tab();
                } else {
                    self.next_tab();
                }
            }
            _ => {}
        }
    }

    fn handle_categories_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.category_focus > 0 {
                    self.category_focus -= 1;
                }
            }
            KeyCode::Down => {
                if self.category_focus < MigrationCategories::LABELS.len() - 1 {
                    self.category_focus += 1;
                }
            }
            KeyCode::Char(' ') => {
                self.categories.toggle(self.category_focus);
            }
            KeyCode::Enter | KeyCode::Tab => {
                if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_tab();
                } else {
                    // Reset plan so it rebuilds with new categories
                    self.plan = None;
                    self.source_data = None;
                    self.next_tab();
                }
            }
            _ => {}
        }
    }

    fn handle_preview_key(&mut self, key: KeyEvent) {
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
                    self.prev_tab();
                } else {
                    self.next_tab();
                }
            }
            _ => {}
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.apply_migration();
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.prev_tab();
            }
            _ => {}
        }
    }
}

// ── Rendering ──

fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &MigrateState,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let layout = Layout::vertical([
            Constraint::Length(LOGO_HEIGHT),
            Constraint::Length(2), // tab bar
            Constraint::Min(5),    // content
            Constraint::Length(1), // footer
        ])
        .split(area);

        render_logo(frame, layout[0]);
        render_tab_bar(frame, layout[1], state);

        match state.tab {
            Tab::Source => render_source(frame, layout[2], state),
            Tab::Categories => render_categories(frame, layout[2], state),
            Tab::Preview => render_preview(frame, layout[2], state),
            Tab::Confirm => render_confirm(frame, layout[2], state),
        }

        render_footer(frame, layout[3], state);
    })?;
    Ok(())
}

fn render_logo(frame: &mut ratatui::Frame, area: Rect) {
    let logo_lines: Vec<Line> = LOGO.lines().map(Line::raw).collect();
    frame.render_widget(
        Paragraph::new(logo_lines).alignment(Alignment::Center),
        area,
    );
}

fn render_tab_bar(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let mut spans = Vec::new();
    spans.push(Span::raw("  "));

    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" > ", theme::dim()));
        }

        let check = if state.tab_completed(*tab) {
            theme::CHECK
        } else {
            " "
        };
        let style = if *tab == state.tab {
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };

        spans.push(Span::styled(format!("{check} {}", tab.label()), style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_source(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let mut lines = Vec::new();

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Select migration source:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    if state.available_sources.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No migration sources detected.",
            Style::default().fg(theme::RED),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Expected directories:",
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            "    ~/.hermes/  (Hermes Agent)",
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            "    ~/.openclaw/  (OpenClaw)",
            theme::dim(),
        )));
    } else {
        for (i, source) in state.available_sources.iter().enumerate() {
            let selected = i == state.selected_source;
            let marker = if selected { ">" } else { " " };
            let style = if selected {
                Style::default().fg(theme::CYAN)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                format!("  {marker} {}", source.label()),
                style,
            )));

            let dir_display = source.data_dir().display().to_string();
            lines.push(Line::from(Span::styled(
                format!("      {dir_display}"),
                theme::dim(),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_categories(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let mut lines = Vec::new();

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Select what to migrate (Space to toggle):",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    for (i, label) in MigrationCategories::LABELS.iter().enumerate() {
        let checked = state.categories.get(i);
        let focused = i == state.category_focus;
        let checkbox = if checked { "[x]" } else { "[ ]" };
        let marker = if focused { ">" } else { " " };
        let style = if focused {
            Style::default().fg(theme::CYAN)
        } else {
            Style::default()
        };

        lines.push(Line::from(Span::styled(
            format!("  {marker} {checkbox} {label}"),
            style,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_preview(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let mut lines = Vec::new();

    lines.push(Line::default());

    let source_label = state
        .selected_source()
        .map(|s| s.label())
        .unwrap_or("Unknown");
    lines.push(Line::from(Span::styled(
        format!("  Migration preview ({source_label}):",),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  Error: {err}"),
            Style::default().fg(theme::RED),
        )));
    } else {
        let visible_height = area.height.saturating_sub(4) as usize;
        let end = (state.preview_scroll + visible_height).min(state.preview_lines.len());
        let start = state.preview_scroll.min(end);

        for line in &state.preview_lines[start..end] {
            let style = if line.contains("(skip:") {
                theme::dim()
            } else if line.ends_with(':') {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(format!("  {line}"), style)));
        }

        if state.preview_lines.len() > visible_height {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "  [{}/{}] Use Up/Down to scroll",
                    state.preview_scroll + 1,
                    state.preview_lines.len()
                ),
                theme::dim(),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_confirm(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let mut lines = Vec::new();

    lines.push(Line::default());

    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(theme::RED),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Press Esc to exit.",
            theme::dim(),
        )));
    } else if let Some(ref plan) = state.plan {
        let active = plan.active_change_count();
        let creds = plan.credentials_to_add.len();
        let files = plan.memory_files.len() + if plan.persona_file.is_some() { 1 } else { 0 };
        let skills = plan.skill_dirs.len();

        lines.push(Line::from(Span::styled(
            "  Ready to apply migration:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        if active > 0 {
            lines.push(Line::from(Span::styled(
                format!("    {active} config change(s)"),
                Style::default(),
            )));
        }
        if creds > 0 {
            lines.push(Line::from(Span::styled(
                format!("    {creds} credential(s)"),
                Style::default(),
            )));
        }
        if files > 0 {
            lines.push(Line::from(Span::styled(
                format!("    {files} file(s) to copy"),
                Style::default(),
            )));
        }
        if skills > 0 {
            lines.push(Line::from(Span::styled(
                format!("    {skills} skill(s)"),
                Style::default(),
            )));
        }

        if plan.is_empty() {
            lines.push(Line::from(Span::styled(
                "    Nothing to migrate.",
                theme::dim(),
            )));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Press Enter to apply, Esc to cancel.",
            Style::default().fg(theme::CYAN),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &MigrateState) {
    let hint = match state.tab {
        Tab::Source if state.available_sources.is_empty() => " Esc: cancel",
        Tab::Source => " Up/Down: select  Enter: next  Esc: cancel",
        Tab::Categories => " Up/Down: move  Space: toggle  Enter: next  Esc: cancel",
        Tab::Preview => " Up/Down: scroll  Enter: next  Shift+Tab: back  Esc: cancel",
        Tab::Confirm => " Enter: apply  Shift+Tab: back  Esc: cancel",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme::dim()))),
        area,
    );
}

// ── Terminal guard ──

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}

// ── Public entry point ──

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = MigrateState::new();

    loop {
        render(&mut terminal, &state)?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == crossterm::event::KeyEventKind::Release {
                    continue;
                }
                state.handle_key(key);
            }
        }

        if state.done || state.cancelled {
            break;
        }
    }

    // Guard drops here, restoring terminal before printing
    drop(_guard);

    if let Some(ref result) = state.result {
        eprintln!("\nMigration complete:");
        if result.config_changes_applied > 0 {
            eprintln!(
                "  {} config change(s) applied",
                result.config_changes_applied
            );
        }
        if result.credentials_added > 0 {
            eprintln!("  {} credential(s) added", result.credentials_added);
        }
        if result.memory_files_copied > 0 {
            eprintln!("  {} memory file(s) copied", result.memory_files_copied);
        }
        if result.persona_copied {
            eprintln!("  Persona copied to IDENTITY.md");
        }
        if result.skills_copied > 0 {
            eprintln!("  {} skill(s) copied", result.skills_copied);
        }
        for warning in &result.warnings {
            eprintln!("  Warning: {warning}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_flow_is_correct() {
        assert_eq!(
            Tab::ALL,
            [Tab::Source, Tab::Categories, Tab::Preview, Tab::Confirm]
        );
    }

    #[test]
    fn test_tab_navigation_forward() {
        let mut state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        assert_eq!(state.tab, Tab::Source);
        state.next_tab();
        assert_eq!(state.tab, Tab::Categories);
        // Can't test further without actual source data
    }

    #[test]
    fn test_tab_navigation_backward() {
        let mut state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        state.tab = Tab::Categories;
        state.prev_tab();
        assert_eq!(state.tab, Tab::Source);
        state.prev_tab();
        assert_eq!(state.tab, Tab::Source); // already at first
    }

    #[test]
    fn test_source_selection() {
        let mut state = MigrateState::new_with_sources(vec![
            MigrationSource::Hermes,
            MigrationSource::OpenClaw,
        ]);
        assert_eq!(state.selected_source, 0);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.selected_source, 1);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.selected_source, 1); // can't go past end

        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.selected_source, 0);
    }

    #[test]
    fn test_category_toggle() {
        let mut state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        state.tab = Tab::Categories;

        assert!(state.categories.get(0)); // config starts true
        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(!state.categories.get(0)); // toggled off
        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(state.categories.get(0)); // toggled back on
    }

    #[test]
    fn test_esc_cancels() {
        let mut state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.cancelled);
    }

    #[test]
    fn test_empty_sources_blocks_advance() {
        let mut state = MigrateState::new_empty();
        state.next_tab();
        assert_eq!(state.tab, Tab::Source); // can't advance
    }

    #[test]
    fn test_tab_completed_checks() {
        let state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        assert!(state.tab_completed(Tab::Source));
        assert!(state.tab_completed(Tab::Categories));
        assert!(!state.tab_completed(Tab::Preview)); // no plan yet
        assert!(!state.tab_completed(Tab::Confirm));
    }

    #[test]
    fn test_category_focus_navigation() {
        let mut state = MigrateState::new_with_sources(vec![MigrationSource::Hermes]);
        state.tab = Tab::Categories;
        assert_eq!(state.category_focus, 0);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.category_focus, 1);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.category_focus, 4);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.category_focus, 4); // can't go past 4

        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.category_focus, 3);
    }

    #[test]
    fn test_selected_source() {
        let state = MigrateState::new_with_sources(vec![
            MigrationSource::Hermes,
            MigrationSource::OpenClaw,
        ]);
        assert_eq!(state.selected_source(), Some(MigrationSource::Hermes));

        let empty = MigrateState::new_empty();
        assert_eq!(empty.selected_source(), None);
    }
}
