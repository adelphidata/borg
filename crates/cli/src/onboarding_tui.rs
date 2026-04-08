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
use crate::onboarding::{models_for_provider, OnboardingResult, PROVIDERS};
use crate::tui::theme;
use borg_core::config::Config;
use borg_core::provider::Provider;
use rand::Rng;
use std::str::FromStr;

const BORG_NAMES: &[&str] = &[
    // People
    "Borgan Freeman",
    "Mark Zuckerborg",
    "Ruth Bader Ginsborg",
    "Goldborg",
    "Bloomborg",
    "Heidegborg",
    "Gutenborg",
    "Borgman",
    "Steven Spielborg",
    "Ingeborg",
    "Victor Borge",
    "Cyborg Jones",
    "Rube Goldborg",
    "Sandborg",
    "Blomborg",
    "Harrisonborg",
    "Lindborg",
    "Stromborg",
    "Borgström",
    "Carlborg",
    "Weinborg",
    "Greenborg",
    "Rosenborg",
    "Sternborg",
    "Feinborg",
    "Borgenstein",
    "Borgward",
    "Borgson",
    "Ellborg",
    // Places
    "Pittsborg",
    "Luxemborg",
    "Salzborg",
    "Harrisborg",
    "Gettysborg",
    "Williamsborg",
    "Johannesborg",
    "Edinborg",
    "Heidelborg",
    "Nuremborg",
    "Borgholm",
    "Saint Petersborg",
    "Marborg",
    "Borgtown",
    "Frostborg",
    // Pop Culture
    "Spongeborg Squarepants",
    "Borger King",
    "Borgcraft",
    "Borg Lightyear",
    "Borginator",
    "The Borg Identity",
    "Borg of the Rings",
    "Borgzilla",
    "Cyborg",
    "Resistance Is Futile McGee",
    "Seven of Nine Lives",
    // Misc
    "Iceborger",
    "Cheeseborger",
    "Hamborg",
    "Borgify",
    "Newborg Baby",
    "Reborged",
    "Borganic",
    "Unborgivable",
    // Additional People
    "Borges",
    "Schoenberg",
    "Borgkovich",
    "Heisenberg",
    "Borgdorf",
    "Wittgenborg",
    "Schrödingborg",
    "Einsteborg",
    "Leonardo da Borgci",
    "Borgart Humphrey",
    "Borggart",
    "Schwarzenegborg",
    "Clint Eastborg",
    "Alfred Hitchborg",
    "Borgothy",
    "Borgatron",
    "Borgleone",
    "Neil Armborg",
    "Amelia Borghart",
    "Marie Borgie",
    "Oppenborger",
    "Borgdwin",
    "Borgsworth",
    "Van Borghen",
    "Remborgdt",
    "Picasborg",
    "Shakespeaborg",
    "Charlesborg",
    "Napoleborg",
    "Cleoborga",
    // Additional Places
    "Gotheborg",
    "Strasborg",
    "Borgdeaux",
    "Canterborg",
    "Middelborg",
    "Augsborg",
    "Borgona",
    "Brandenborg",
    "Regenborg",
    "Meckleborg",
    "Borggundy",
    "Borgcelona",
    "Flenborg",
    "Magdeborg",
    "Borgamo",
    // Additional Pop Culture
    "Borgpool",
    "Borg Vader",
    "Obi-Wan Kenoborg",
    "Gandborg",
    "Frodo Borggins",
    "Borglock Holmes",
    "James Borgd",
    "Indiana Borgs",
    "Captain Borgmerica",
    "The Incredible Borg",
    "Borg Panther",
    "Borg to the Future",
    "Jurassic Borg",
    "The Borgfather",
    "Borgbusters",
    "Borg Wars",
    "The Borgrix",
    "Borgman Begins",
    "Doctor Borgenstein",
    "Frankenborg",
    // Additional Misc
    "Borgalicious",
    "Borgmeister",
    "Borgnado",
    "Borgtastic",
    "Borgpocalypse",
    "Turborg",
    "Borgberry",
    "Borgasaurus",
    "Borgnarok",
    "El Borgo",
    "Borgchamp",
    "Borgsmith",
    "Borgolithic",
    "Borganaut",
    "Borgopolis",
];

fn random_borg_name() -> String {
    let idx = rand::rng().random_range(0..BORG_NAMES.len());
    BORG_NAMES[idx].to_string()
}

const LOGO_HEIGHT: u16 = 8; // includes leading blank line

const SECURITY_TITLE: &str = "SECURITY WARNING - PLEASE READ";

const SECURITY_BODY: &str = "\
Borg is a personal AI agent that can execute tools and shell commands.
By default, it operates as a single trusted-operator system.

If you expose Borg to multiple users (e.g. via gateway channels),
each user shares the agent's delegated tool authority.

Recommended baseline:
  • Sandbox mode enabled (strict by default)
  • Least-privilege tools — don't grant unnecessary fs/network access
  • Keep secrets out of the agent's reachable filesystem
  • Shared use: isolate sessions per user/channel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Welcome,
    Security,
    Provider,
    ApiKey,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Welcome, Tab::Security, Tab::Provider, Tab::ApiKey];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Welcome => "Welcome",
            Tab::Security => "Disclaimer",
            Tab::Provider => "Provider",
            Tab::ApiKey => "API Key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WelcomeFocus {
    UserName,
    AgentName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    TextInput,
}

fn is_shift_tab(key: &KeyEvent) -> bool {
    key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT)
}

struct OnboardingState {
    tab: Tab,
    input_mode: InputMode,

    // Welcome
    user_name: String,
    agent_name: String,
    default_agent_name: String,
    welcome_focus: WelcomeFocus,

    // Security
    security_accepted: bool,

    // Provider
    provider_index: usize,
    claude_cli_detected: bool,

    // API Key
    api_key_input: String,
    api_key_existing: bool,

    // Validation hints
    api_key_required_hint: bool,

    // Result
    done: bool,
    cancelled: bool,
}

impl OnboardingState {
    fn new() -> Self {
        let claude_cli_detected = borg_core::claude_cli::has_valid_auth();

        let api_key_existing = PROVIDERS
            .iter()
            .any(|(id, _, _)| Self::check_existing_api_key_cached(id, claude_cli_detected));

        // Auto-select Claude CLI if detected, otherwise default to first provider
        let provider_index = if claude_cli_detected {
            PROVIDERS
                .iter()
                .position(|(id, _, _)| *id == "claude-cli")
                .unwrap_or(0)
        } else {
            0
        };

        let default_name = random_borg_name();
        Self {
            tab: Tab::Welcome,
            input_mode: InputMode::TextInput,
            user_name: String::new(),
            agent_name: default_name.clone(),
            default_agent_name: default_name,
            welcome_focus: WelcomeFocus::UserName,
            security_accepted: false,
            provider_index,
            claude_cli_detected,
            api_key_input: String::new(),
            api_key_existing,
            api_key_required_hint: false,
            done: false,
            cancelled: false,
        }
    }

    fn check_existing_api_key_cached(provider_id: &str, claude_cli_detected: bool) -> bool {
        // Claude CLI: use cached detection result
        if provider_id == "claude-cli" {
            return claude_cli_detected;
        }
        let Ok(provider) = Provider::from_str(provider_id) else {
            return false;
        };
        // Keyless providers: check if server is reachable instead of API key
        if !provider.requires_api_key() {
            return Provider::ollama_available();
        }
        let env_var_name = provider.default_env_var();
        let Ok(data_dir) = Config::data_dir() else {
            return false;
        };
        let env_path = data_dir.join(".env");
        if !env_path.exists() {
            return false;
        }
        std::fs::read_to_string(&env_path)
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    line.strip_prefix(&format!("{env_var_name}="))
                        .map(|v| v.trim().trim_matches('"').to_string())
                })
            })
            .is_some_and(|k| !k.is_empty())
    }

    #[cfg(test)]
    fn detect_provider_from_key(key: &str) -> &'static str {
        let trimmed = key.trim();
        if trimmed.starts_with("sk-or-") {
            "openrouter"
        } else if trimmed.starts_with("sk-ant-") {
            "anthropic"
        } else if trimmed.starts_with("sk-") {
            "openai"
        } else if trimmed.starts_with("AIza") {
            "gemini"
        } else {
            "openrouter"
        }
    }

    #[cfg(test)]
    fn detect_provider_from_env(&self) -> &'static str {
        for (id, _, _) in PROVIDERS {
            if *id == "claude-cli" {
                if borg_core::claude_cli::has_valid_auth() {
                    return id;
                }
                continue;
            }
            if let Ok(p) = Provider::from_str(id) {
                if !p.requires_api_key() {
                    // Keyless providers: detect by checking if server is reachable
                    if Provider::ollama_available() {
                        return id;
                    }
                    continue;
                }
                if std::env::var(p.default_env_var())
                    .ok()
                    .is_some_and(|k| !k.is_empty())
                {
                    return id;
                }
            }
        }
        "openrouter"
    }

    fn tab_completed(&self, tab: Tab) -> bool {
        match tab {
            Tab::Welcome => !self.user_name.is_empty(),
            Tab::Security => self.security_accepted,
            Tab::Provider => true, // Always has a selection
            Tab::ApiKey => false,
        }
    }

    /// Whether the selected provider needs an API key.
    fn selected_provider_needs_key(&self) -> bool {
        let (id, _, _) = PROVIDERS[self.provider_index];
        id != "claude-cli" && id != "ollama"
    }

    fn build_result(&self) -> OnboardingResult {
        let (provider_id, _, _) = PROVIDERS[self.provider_index];

        let api_key = if !self.selected_provider_needs_key()
            || self.api_key_input.trim().is_empty()
            || self.api_key_existing
        {
            None
        } else {
            Some(self.api_key_input.trim().to_string())
        };

        let models = models_for_provider(provider_id);
        let model_id = models[0].0.to_string();

        OnboardingResult {
            user_name: self.user_name.clone(),
            agent_name: if self.agent_name.is_empty() {
                self.default_agent_name.clone()
            } else {
                self.agent_name.clone()
            },
            model_id,
            api_key,
            provider: provider_id.to_string(),
        }
    }

    fn next_tab(&mut self) {
        let idx = self.tab.index();
        if idx < Tab::ALL.len() - 1 {
            match self.tab {
                Tab::Welcome if self.user_name.trim().is_empty() => return,
                Tab::Security if !self.security_accepted => return,
                Tab::Provider if !self.selected_provider_needs_key() => {
                    // Skip API Key tab for keyless providers — go straight to done
                    self.done = true;
                    return;
                }
                _ => {}
            }
            self.tab = Tab::ALL[idx + 1];
            self.update_input_mode();
        }
    }

    fn prev_tab(&mut self) {
        let idx = self.tab.index();
        if idx > 0 {
            self.tab = Tab::ALL[idx - 1];
            self.update_input_mode();
        }
    }

    fn update_input_mode(&mut self) {
        self.input_mode = match self.tab {
            Tab::Welcome => InputMode::TextInput,
            Tab::ApiKey if !self.api_key_existing => InputMode::TextInput,
            Tab::Provider | Tab::Security => InputMode::Normal,
            _ => InputMode::Normal,
        };
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancelled = true;
            return;
        }

        match self.tab {
            Tab::Welcome => self.handle_welcome_key(key),
            Tab::Security => self.handle_security_key(key),
            Tab::Provider => self.handle_provider_key(key),
            Tab::ApiKey => self.handle_api_key_key(key),
        }
    }

    fn handle_welcome_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                if is_shift_tab(&key) {
                    self.prev_tab();
                } else {
                    match self.welcome_focus {
                        WelcomeFocus::UserName => {
                            self.welcome_focus = WelcomeFocus::AgentName;
                        }
                        WelcomeFocus::AgentName => {
                            self.next_tab();
                        }
                    }
                }
            }
            KeyCode::Right => self.next_tab(),
            KeyCode::Left => self.prev_tab(),
            KeyCode::Enter => match self.welcome_focus {
                WelcomeFocus::UserName => {
                    self.welcome_focus = WelcomeFocus::AgentName;
                }
                WelcomeFocus::AgentName => {
                    self.next_tab();
                }
            },
            KeyCode::Backspace => {
                let buf = match self.welcome_focus {
                    WelcomeFocus::UserName => &mut self.user_name,
                    WelcomeFocus::AgentName => &mut self.agent_name,
                };
                buf.pop();
            }
            KeyCode::Char(c) => {
                let buf = match self.welcome_focus {
                    WelcomeFocus::UserName => &mut self.user_name,
                    WelcomeFocus::AgentName => &mut self.agent_name,
                };
                buf.push(c);
            }
            _ => {}
        }
    }

    fn handle_security_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.security_accepted = true;
                self.next_tab();
            }
            KeyCode::Tab | KeyCode::Right => {
                if is_shift_tab(&key) {
                    self.prev_tab();
                } else {
                    self.security_accepted = true;
                    self.next_tab();
                }
            }
            KeyCode::Left => self.prev_tab(),
            _ => {}
        }
    }

    fn handle_provider_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.provider_index > 0 {
                    self.provider_index -= 1;
                } else {
                    self.provider_index = PROVIDERS.len() - 1;
                }
            }
            KeyCode::Down => {
                self.provider_index = (self.provider_index + 1) % PROVIDERS.len();
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => {
                if is_shift_tab(&key) {
                    self.prev_tab();
                } else {
                    self.next_tab();
                }
            }
            KeyCode::Left => self.prev_tab(),
            _ => {}
        }
    }

    fn handle_api_key_key(&mut self, key: KeyEvent) {
        if self.api_key_existing {
            match key.code {
                KeyCode::Tab | KeyCode::Right => {
                    if is_shift_tab(&key) {
                        self.prev_tab();
                    } else {
                        self.done = true;
                    }
                }
                KeyCode::Left => self.prev_tab(),
                KeyCode::Enter => {
                    self.done = true;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Tab => {
                if is_shift_tab(&key) {
                    self.prev_tab();
                } else if self.api_key_input.trim().is_empty() {
                    self.api_key_required_hint = true;
                } else {
                    self.api_key_required_hint = false;
                    self.done = true;
                }
            }
            KeyCode::Right => {
                if self.api_key_input.trim().is_empty() {
                    self.api_key_required_hint = true;
                } else {
                    self.api_key_required_hint = false;
                    self.done = true;
                }
            }
            KeyCode::Left => self.prev_tab(),
            KeyCode::Enter => {
                if self.api_key_input.trim().is_empty() {
                    self.api_key_required_hint = true;
                } else {
                    self.api_key_required_hint = false;
                    self.done = true;
                }
            }
            KeyCode::Backspace => {
                self.api_key_input.pop();
                self.api_key_required_hint = false;
            }
            KeyCode::Char(c) => {
                self.api_key_input.push(c);
                self.api_key_required_hint = false;
            }
            _ => {}
        }
    }
}

// ── Rendering ──

fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &OnboardingState,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        if area.width < 60 || area.height < 20 {
            let msg = Paragraph::new("Please resize your terminal to at least 60x20")
                .alignment(Alignment::Center);
            let y = area.height / 2;
            let msg_area = Rect::new(0, y, area.width, 1);
            frame.render_widget(msg, msg_area);
            return;
        }

        let chunks = Layout::vertical([
            Constraint::Length(LOGO_HEIGHT + 1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

        render_logo(frame, chunks[0]);
        render_tab_bar(frame, chunks[1], state);
        render_tab_content(frame, chunks[2], state);
        render_footer(frame, chunks[3], state);
    })?;
    Ok(())
}

fn render_logo(frame: &mut ratatui::Frame, area: Rect) {
    let lines: Vec<Line> = LOGO
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    let max_width = LOGO.lines().map(str::len).max().unwrap_or(0) as u16;
    let logo_width = max_width.min(area.width);
    let x_offset = area.x + area.width.saturating_sub(logo_width) / 2;
    let centered_area = Rect::new(x_offset, area.y, logo_width, area.height);
    let logo = Paragraph::new(lines).alignment(Alignment::Left);
    frame.render_widget(logo, centered_area);
}

fn render_tab_bar(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw("  "));

    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", theme::dim()));
        }

        let completed = state.tab_completed(*tab) && *tab != state.tab;
        let prefix = if completed { "✓ " } else { "" };
        let label = format!("{prefix}{}", tab.label());

        let style = if *tab == state.tab {
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else if completed {
            Style::default().fg(theme::GREEN)
        } else {
            theme::dim()
        };

        spans.push(Span::styled(label, style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_tab_content(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let content_area = Rect::new(
        area.x + 4,
        area.y + 1,
        area.width.saturating_sub(8),
        area.height.saturating_sub(2),
    );

    match state.tab {
        Tab::Welcome => render_welcome(frame, content_area, state),
        Tab::Security => render_security(frame, content_area, state),
        Tab::Provider => render_provider(frame, content_area, state),
        Tab::ApiKey => render_api_key(frame, content_area, state),
    }
}

fn render_welcome(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Let's set up your personal AI assistant.",
        Style::default().fg(theme::CYAN),
    )));
    lines.push(Line::default());
    lines.push(Line::default());

    // User name field
    let user_name_focused = state.welcome_focus == WelcomeFocus::UserName;
    let user_label_style = if user_name_focused {
        Style::default()
            .fg(theme::CYAN)
            .add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    };
    lines.push(Line::from(Span::styled(
        "  What's your name?",
        user_label_style,
    )));
    let user_cursor = if user_name_focused { "▊" } else { "" };
    let user_border_style = if user_name_focused {
        theme::CYAN
    } else {
        theme::DIM_WHITE
    };
    lines.push(Line::from(vec![
        Span::styled("  › ", Style::default().fg(user_border_style)),
        Span::raw(&state.user_name),
        Span::styled(user_cursor, Style::default().fg(theme::CYAN)),
    ]));

    lines.push(Line::default());
    lines.push(Line::default());

    // Agent name field
    let agent_name_focused = state.welcome_focus == WelcomeFocus::AgentName;
    let agent_label_style = if agent_name_focused {
        Style::default()
            .fg(theme::CYAN)
            .add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    };
    lines.push(Line::from(Span::styled(
        "  What's your agent's name?",
        agent_label_style,
    )));
    let agent_cursor = if agent_name_focused { "▊" } else { "" };
    let agent_border_style = if agent_name_focused {
        theme::CYAN
    } else {
        theme::DIM_WHITE
    };
    lines.push(Line::from(vec![
        Span::styled("  › ", Style::default().fg(agent_border_style)),
        Span::raw(&state.agent_name),
        Span::styled(agent_cursor, Style::default().fg(theme::CYAN)),
    ]));

    if state.user_name.trim().is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Enter your name to continue",
            theme::dim(),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_security(frame: &mut ratatui::Frame, area: Rect, _state: &OnboardingState) {
    let mut lines: Vec<Line> = Vec::new();

    // Title in bold white
    lines.push(Line::from(Span::styled(
        format!("  {SECURITY_TITLE}"),
        Style::default()
            .fg(ratatui::style::Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    // Body in yellow
    for line in SECURITY_BODY.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(theme::YELLOW),
        )));
    }

    lines.push(Line::default());

    // "Run diagnostics anytime: borg doctor" with bold white on "borg doctor"
    lines.push(Line::from(vec![
        Span::styled(
            "  Run diagnostics anytime: ",
            Style::default().fg(theme::YELLOW),
        ),
        Span::styled(
            "borg doctor",
            Style::default()
                .fg(ratatui::style::Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    lines.push(Line::default());
    lines.push(Line::default());

    lines.push(Line::from(Span::styled(
        "  Press Enter to continue",
        theme::dim(),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_provider(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Choose your LLM provider",
        Style::default().fg(theme::CYAN),
    )));
    lines.push(Line::default());

    for (i, (id, label, desc)) in PROVIDERS.iter().enumerate() {
        let is_selected = i == state.provider_index;
        let indicator = if is_selected { "● " } else { "○ " };
        let style = if is_selected {
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };

        // Show availability hint for keyless providers
        let availability = if *id == "claude-cli" {
            if state.claude_cli_detected {
                " ✓"
            } else {
                " (not detected)"
            }
        } else if *id == "ollama" {
            if Provider::ollama_available() {
                " ✓"
            } else {
                " (not running)"
            }
        } else {
            ""
        };

        lines.push(Line::from(vec![
            Span::styled(format!("  {indicator}{label}"), style),
            Span::styled(
                format!("  {desc}{availability}"),
                if is_selected {
                    Style::default().fg(theme::DIM_WHITE)
                } else {
                    theme::dim()
                },
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  ↑/↓: select  Enter: continue",
        theme::dim(),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_api_key(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Paste your API key",
        Style::default().fg(theme::CYAN),
    )));
    lines.push(Line::default());

    if state.api_key_existing {
        lines.push(Line::from(Span::styled(
            "  ✓ API key already configured",
            Style::default().fg(theme::GREEN),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Press Tab or Enter to continue",
            theme::dim(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Get one at openrouter.ai/keys — one key, all models",
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            "  OpenAI, Anthropic, and Gemini keys also work",
            theme::dim(),
        )));
        lines.push(Line::default());

        let masked: String = "*".repeat(state.api_key_input.len());
        lines.push(Line::from(vec![
            Span::styled("  › ", Style::default().fg(theme::CYAN)),
            Span::raw(masked),
            Span::styled("▊", Style::default().fg(theme::CYAN)),
        ]));

        if state.api_key_required_hint {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  ⚠ Please enter your API key to continue",
                Style::default().fg(theme::YELLOW),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let hint = match state.tab {
        Tab::Welcome => " Tab: next field  Enter: next  Esc: cancel",
        Tab::Provider => " ↑/↓: select  Enter/Tab: next  Shift+Tab: back  Esc: cancel",
        _ if state.input_mode == InputMode::TextInput => " Type to enter  Tab: next  Esc: cancel",
        _ => " Tab/→: next  Shift+Tab/←: back  Esc: cancel",
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

pub fn run() -> Result<Option<OnboardingResult>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = OnboardingState::new();

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

        if state.done {
            return Ok(Some(state.build_result()));
        }
        if state.cancelled {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_flow_is_correct() {
        assert_eq!(
            Tab::ALL,
            [Tab::Welcome, Tab::Security, Tab::Provider, Tab::ApiKey,]
        );
    }

    #[test]
    fn tab_navigation_forward() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        // Select a provider that requires API key so we don't skip to done
        state.provider_index = 0; // OpenRouter
        assert_eq!(state.tab, Tab::Welcome);
        state.next_tab();
        assert_eq!(state.tab, Tab::Security);
        state.next_tab();
        assert_eq!(state.tab, Tab::Provider);
        state.next_tab();
        assert_eq!(state.tab, Tab::ApiKey);
    }

    #[test]
    fn tab_navigation_backward() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Security;
        state.prev_tab();
        assert_eq!(state.tab, Tab::Welcome);
        state.prev_tab();
        assert_eq!(state.tab, Tab::Welcome);
    }

    #[test]
    fn security_blocks_without_acceptance() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.tab = Tab::Security;
        state.security_accepted = false;
        state.next_tab();
        assert_eq!(state.tab, Tab::Security); // should not advance
    }

    #[test]
    fn security_allows_with_acceptance() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.tab = Tab::Security;
        state.security_accepted = true;
        state.next_tab();
        assert_eq!(state.tab, Tab::Provider);
    }

    #[test]
    fn empty_name_blocks_advance() {
        let mut state = OnboardingState::new();
        state.user_name.clear();
        state.next_tab();
        assert_eq!(state.tab, Tab::Welcome);
    }

    #[test]
    fn build_result_uses_selected_provider() {
        let mut state = OnboardingState::new();
        state.user_name = "Alice".to_string();
        state.agent_name = "Buddy".to_string();
        state.provider_index = 0; // OpenRouter

        let result = state.build_result();
        assert_eq!(result.user_name, "Alice");
        assert_eq!(result.agent_name, "Buddy");
        assert_eq!(result.provider, "openrouter");
    }

    #[test]
    fn build_result_uses_explicit_provider_selection() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        // Select Anthropic (index 2 in PROVIDERS)
        state.provider_index = 2;
        state.api_key_input = "sk-ant-test123".to_string();
        let result = state.build_result();
        assert_eq!(result.provider, "anthropic");
    }

    #[test]
    fn empty_agent_name_defaults_to_random_borg_name() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        let expected = state.default_agent_name.clone();
        state.agent_name.clear();
        let result = state.build_result();
        assert_eq!(
            result.agent_name, expected,
            "Empty agent_name should fall back to the initially generated default"
        );
    }

    #[test]
    fn borg_names_array_is_non_empty() {
        assert!(
            BORG_NAMES.len() >= 100,
            "Expected at least 100 Borg names, got: {}",
            BORG_NAMES.len()
        );
    }

    #[test]
    fn borg_names_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for name in BORG_NAMES {
            assert!(seen.insert(name), "Duplicate Borg name: {name}");
        }
    }

    #[test]
    fn random_borg_name_returns_name_from_list() {
        for _ in 0..20 {
            let name = random_borg_name();
            assert!(
                BORG_NAMES.contains(&name.as_str()),
                "random_borg_name() returned unknown name: {name}"
            );
        }
    }

    #[test]
    fn initial_agent_name_is_from_borg_names() {
        let state = OnboardingState::new();
        assert!(
            BORG_NAMES.contains(&state.agent_name.as_str()),
            "Initial agent_name not in BORG_NAMES: {}",
            state.agent_name
        );
    }

    #[test]
    fn esc_sets_cancelled() {
        let mut state = OnboardingState::new();
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.cancelled);
    }

    #[test]
    fn tab_completed_checks() {
        let mut state = OnboardingState::new();
        assert!(!state.tab_completed(Tab::Welcome)); // empty name
        state.user_name = "Test".to_string();
        assert!(state.tab_completed(Tab::Welcome));
        assert!(!state.tab_completed(Tab::Security)); // not accepted
        state.security_accepted = true;
        assert!(state.tab_completed(Tab::Security));
        assert!(!state.tab_completed(Tab::ApiKey));
    }

    #[test]
    fn detect_provider_from_key_prefixes() {
        assert_eq!(
            OnboardingState::detect_provider_from_key("sk-or-abc"),
            "openrouter"
        );
        assert_eq!(
            OnboardingState::detect_provider_from_key("sk-ant-abc"),
            "anthropic"
        );
        assert_eq!(
            OnboardingState::detect_provider_from_key("sk-abc"),
            "openai"
        );
        assert_eq!(
            OnboardingState::detect_provider_from_key("AIzaXYZ"),
            "gemini"
        );
        assert_eq!(
            OnboardingState::detect_provider_from_key("unknown"),
            "openrouter"
        );
    }

    // ── Provider tab tests ──

    #[test]
    fn provider_tab_navigation_up_down() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Provider;
        state.provider_index = 0;

        // Down moves forward
        state.handle_provider_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.provider_index, 1);

        // Up moves backward
        state.handle_provider_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.provider_index, 0);

        // Up wraps to last
        state.handle_provider_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.provider_index, PROVIDERS.len() - 1);

        // Down wraps to first
        state.handle_provider_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.provider_index, 0);
    }

    #[test]
    fn keyless_provider_skips_api_key_tab() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        state.tab = Tab::Provider;

        // Find claude-cli index
        let cli_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "claude-cli")
            .unwrap();
        state.provider_index = cli_idx;

        // next_tab from Provider with keyless provider should set done=true
        state.next_tab();
        assert!(state.done);
    }

    #[test]
    fn api_key_provider_goes_to_api_key_tab() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        state.tab = Tab::Provider;
        state.provider_index = 0; // OpenRouter needs API key

        state.next_tab();
        assert_eq!(state.tab, Tab::ApiKey);
        assert!(!state.done);
    }

    #[test]
    fn build_result_claude_cli_no_api_key() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        let cli_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "claude-cli")
            .unwrap();
        state.provider_index = cli_idx;
        state.api_key_input = "should-be-ignored".to_string();

        let result = state.build_result();
        assert_eq!(result.provider, "claude-cli");
        assert!(result.api_key.is_none()); // No API key for keyless provider
    }

    #[test]
    fn selected_provider_needs_key_logic() {
        let mut state = OnboardingState::new();

        // OpenRouter needs key
        state.provider_index = 0;
        assert!(state.selected_provider_needs_key());

        // Claude CLI doesn't need key
        let cli_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "claude-cli")
            .unwrap();
        state.provider_index = cli_idx;
        assert!(!state.selected_provider_needs_key());

        // Ollama doesn't need key
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        assert!(!state.selected_provider_needs_key());
    }

    #[test]
    fn provider_tab_completed_always_true() {
        let state = OnboardingState::new();
        assert!(state.tab_completed(Tab::Provider));
    }
}
