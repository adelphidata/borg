//! "Update available" modal shown on TUI start when a newer release is on
//! GitHub. Modeled on Codex's `update_prompt.rs`, trimmed to Borg's existing
//! TUI primitives (synchronous ratatui draw loop, no `FrameRequester`).
//!
//! The modal owns its own raw-mode + alt-screen lifecycle so the caller can
//! invoke it before the main TUI is wired up. On user choice:
//! - `UpdateNow`  → returns `RunUpdate`, caller dispatches the existing
//!   `/update` flow (no command duplication).
//! - `NotNow` / Esc / Ctrl+C → returns `Continue`, no DB write.
//! - `DontRemind` → persists `update.skipped_version = <latest>` so the modal
//!   suppresses itself until a newer release lands.
//!
//! In debug builds the entry point short-circuits to `Continue` — matches
//! Codex parity and keeps `cargo run` from gating on every dev iteration.
//!
//! Network / RPC failures never propagate: the agent always boots.

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use cortex::config::Config;
use cortex::update::{
    self, current_version, detect_install_source, update_command_str, RELEASE_NOTES_URL,
};

use crate::daemon_client;
use crate::terminal_guard::TerminalGuard;
use crate::tui::theme;

/// Outcome handed back to the TUI launcher.
pub(crate) enum UpdatePromptOutcome {
    /// No prompt was shown, or user picked Skip / Skip-version. Continue boot.
    Continue,
    /// User picked "Update now" — caller should dispatch the existing
    /// `/update` install flow.
    RunUpdate,
}

/// Probe GitHub, decide whether to show the prompt, and run the modal.
///
/// Returns `Continue` on:
/// - debug builds (modal disabled),
/// - no network / GitHub error,
/// - already up-to-date,
/// - user has dismissed this exact version via "Skip until next version",
/// - user picks Skip / Esc / Ctrl+C.
pub(crate) async fn run_update_prompt_if_needed(config: &Config) -> Result<UpdatePromptOutcome> {
    // Disabled in debug builds so dev iterations aren't gated on every run.
    if cfg!(debug_assertions) {
        return Ok(UpdatePromptOutcome::Continue);
    }

    let release = match update::fetch_latest_release(false).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("update prompt: skipping — fetch_latest_release failed: {e:#}");
            return Ok(UpdatePromptOutcome::Continue);
        }
    };
    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_string();
    let current = current_version().to_string();

    if !should_show(&current, &latest, &config.update.skipped_version) {
        return Ok(UpdatePromptOutcome::Continue);
    }

    let source = detect_install_source();
    let selection = match run_modal(&current, &latest, source) {
        Ok(s) => s,
        Err(e) => {
            // A TTY/raw-mode/draw failure must never block boot. Log and
            // continue — same policy as the GitHub fetch failure above.
            tracing::warn!("update prompt: modal failed, continuing boot: {e:#}");
            return Ok(UpdatePromptOutcome::Continue);
        }
    };

    match selection {
        UpdateSelection::UpdateNow => Ok(UpdatePromptOutcome::RunUpdate),
        UpdateSelection::NotNow => Ok(UpdatePromptOutcome::Continue),
        UpdateSelection::DontRemind => {
            persist_skipped_version(&latest).await;
            Ok(UpdatePromptOutcome::Continue)
        }
    }
}

/// Pure decision: should the modal appear?
///
/// Re-prompts when a newer-than-skipped version lands — the whole point of
/// "Skip until next version".
fn should_show(current: &str, latest: &str, skipped: &str) -> bool {
    if !update::is_newer(current, latest) {
        return false;
    }
    if !skipped.is_empty() && skipped == latest {
        return false;
    }
    true
}

async fn persist_skipped_version(latest: &str) {
    match daemon_client::connect_or_spawn().await {
        Ok(mut client) => {
            if let Err(e) = client.set_setting("update.skipped_version", latest).await {
                tracing::warn!("update prompt: failed to persist skipped_version: {e:#}");
            }
        }
        Err(e) => {
            tracing::warn!("update prompt: connect_or_spawn failed: {e:#}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateSelection {
    UpdateNow,
    NotNow,
    DontRemind,
}

impl UpdateSelection {
    fn next(self) -> Self {
        match self {
            UpdateSelection::UpdateNow => UpdateSelection::NotNow,
            UpdateSelection::NotNow => UpdateSelection::DontRemind,
            UpdateSelection::DontRemind => UpdateSelection::UpdateNow,
        }
    }

    fn prev(self) -> Self {
        match self {
            UpdateSelection::UpdateNow => UpdateSelection::DontRemind,
            UpdateSelection::NotNow => UpdateSelection::UpdateNow,
            UpdateSelection::DontRemind => UpdateSelection::NotNow,
        }
    }
}

struct PromptState {
    highlighted: UpdateSelection,
    selection: Option<UpdateSelection>,
}

impl PromptState {
    fn new() -> Self {
        Self {
            highlighted: UpdateSelection::UpdateNow,
            selection: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.select(UpdateSelection::NotNow);
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.highlighted = self.highlighted.prev(),
            KeyCode::Down | KeyCode::Char('j') => self.highlighted = self.highlighted.next(),
            KeyCode::Char('1') => self.select(UpdateSelection::UpdateNow),
            KeyCode::Char('2') => self.select(UpdateSelection::NotNow),
            KeyCode::Char('3') => self.select(UpdateSelection::DontRemind),
            KeyCode::Enter => self.select(self.highlighted),
            KeyCode::Esc => self.select(UpdateSelection::NotNow),
            _ => {}
        }
    }

    fn select(&mut self, sel: UpdateSelection) {
        self.highlighted = sel;
        self.selection = Some(sel);
    }
}

/// Run the modal, returning the user's selection. Owns its own raw-mode +
/// alt-screen lifecycle (RAII guard on Drop) so it can run before the main
/// TUI is initialized.
fn run_modal(
    current: &str,
    latest: &str,
    source: update::InstallSource,
) -> Result<UpdateSelection> {
    enable_raw_mode()?;
    // Construct the guard BEFORE entering alt-screen so a failed
    // EnterAlternateScreen still restores raw-mode on Drop. The guard's
    // LeaveAlternateScreen is harmless if alt-screen was never entered.
    let _guard = TerminalGuard;
    stdout().execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = PromptState::new();

    loop {
        terminal.draw(|frame| {
            render(frame, frame.area(), current, latest, source, &state);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                state.handle_key(key);
            }
        }

        if let Some(sel) = state.selection {
            return Ok(sel);
        }
    }
}

fn render(
    frame: &mut ratatui::Frame,
    area: Rect,
    current: &str,
    latest: &str,
    source: update::InstallSource,
    state: &PromptState,
) {
    if area.width < 40 || area.height < 10 {
        let msg = Paragraph::new("Resize terminal to see update prompt").alignment(Alignment::Left);
        frame.render_widget(msg, area);
        return;
    }

    let cmd = update_command_str(source);
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            "  ✨ ",
            Style::default()
                .fg(theme::AMBER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Update available!", theme::bold()),
        Span::raw(" "),
        Span::styled(format!("{current} -> {latest}"), theme::dim()),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Release notes: ", theme::dim()),
        Span::styled(
            RELEASE_NOTES_URL,
            theme::dim().add_modifier(Modifier::UNDERLINED),
        ),
    ]));
    lines.push(Line::default());

    lines.push(option_row(
        1,
        format!("Update now (runs `{cmd}`)"),
        state.highlighted == UpdateSelection::UpdateNow,
    ));
    lines.push(option_row(
        2,
        "Skip".to_string(),
        state.highlighted == UpdateSelection::NotNow,
    ));
    lines.push(option_row(
        3,
        "Skip until next version".to_string(),
        state.highlighted == UpdateSelection::DontRemind,
    ));

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Press ", theme::dim()),
        Span::styled("enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" to continue", theme::dim()),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn option_row(num: u8, label: String, highlighted: bool) -> Line<'static> {
    let pointer = if highlighted { "›" } else { " " };
    let style = if highlighted {
        Style::default()
            .fg(theme::CYAN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{pointer} "), Style::default().fg(theme::CYAN)),
        Span::styled(format!("{num}. {label}"), style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_on_default_selects_update_now() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.selection, Some(UpdateSelection::UpdateNow));
    }

    #[test]
    fn down_down_enter_selects_dont_remind() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.selection, Some(UpdateSelection::DontRemind));
    }

    #[test]
    fn esc_skips_without_dismissal() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Esc));
        // NotNow ≠ DontRemind: Esc must not persist a skipped version.
        assert_eq!(s.selection, Some(UpdateSelection::NotNow));
    }

    #[test]
    fn ctrl_c_skips_without_dismissal() {
        let mut s = PromptState::new();
        s.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(s.selection, Some(UpdateSelection::NotNow));
    }

    #[test]
    fn ctrl_d_skips_without_dismissal() {
        let mut s = PromptState::new();
        s.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(s.selection, Some(UpdateSelection::NotNow));
    }

    #[test]
    fn number_shortcuts_jump_select_regardless_of_highlight() {
        // Highlight is on UpdateNow; pressing 3 must still select DontRemind
        // without an intermediate Down/Enter dance.
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Char('3')));
        assert_eq!(s.selection, Some(UpdateSelection::DontRemind));

        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Char('2')));
        assert_eq!(s.selection, Some(UpdateSelection::NotNow));
    }

    #[test]
    fn up_from_first_wraps_to_last() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.highlighted, UpdateSelection::DontRemind);
    }

    #[test]
    fn down_from_last_wraps_to_first() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.highlighted, UpdateSelection::DontRemind);
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.highlighted, UpdateSelection::UpdateNow);
    }

    #[test]
    fn vim_keys_navigate_like_arrows() {
        let mut s = PromptState::new();
        s.handle_key(key(KeyCode::Char('j')));
        assert_eq!(s.highlighted, UpdateSelection::NotNow);
        s.handle_key(key(KeyCode::Char('k')));
        assert_eq!(s.highlighted, UpdateSelection::UpdateNow);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut s = PromptState::new();
        let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        s.handle_key(release);
        assert!(s.selection.is_none(), "Release events must not select");
    }

    #[test]
    fn should_show_suppresses_when_not_newer() {
        assert!(!should_show("0.3.0", "0.3.0", ""));
        assert!(!should_show("0.4.0", "0.3.0", ""));
    }

    #[test]
    fn should_show_when_newer_and_no_dismissal() {
        assert!(should_show("0.2.0", "0.3.0", ""));
    }

    #[test]
    fn should_show_suppresses_when_dismissed_matches_latest() {
        assert!(!should_show("0.2.0", "0.3.0", "0.3.0"));
    }

    #[test]
    fn should_show_re_prompts_when_newer_release_lands() {
        // The whole point of "Skip until next version": once a newer tag
        // lands, the dismissal no longer applies.
        assert!(should_show("0.2.0", "0.4.0", "0.3.0"));
    }
}
