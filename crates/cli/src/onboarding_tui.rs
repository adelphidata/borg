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
    "Borgan Freeman",
    "Borgbacue",
    "Mark Zuckerborg",
    "Ruth Bader Ginsborg",
    "Borg Washington",
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
    "Blomborg",
    "Harrisonborg",
    "Stromborg",
    "Borgström",
    "Weinborg",
    "Feinborg",
    "Borgenstein",
    "Borgward",
    "Borgson",
    "Ellborg",
    "Pittsborg",
    "Luxemborg",
    "Salzborg",
    "Harrisborg",
    "Gettysborg",
    "Williamsborg",
    "Pittsborg Steelers",
    "Pittsborg Pirates",
    "Pittsborg Penguins",
    "Johannesborg",
    "Nuremborg",
    "Borgholm",
    "Saint Petersborg",
    "Borgtown",
    "Spongeborg Squarepants",
    "Borger King",
    "Borgcraft",
    "Borg Lightyear",
    "Borginator",
    "The Borg Identity",
    "Borg of the Rings",
    "Brown vs the Borg of Education",
    "Borgzilla",
    "Cyborg",
    "Iceborger",
    "Cheeseborger",
    "Hamborg",
    "Borgify",
    "Newborg Baby",
    "Reborged",
    "Borganic",
    "Unborgivable",
    "The Battle of The Borg",
    "Borga Borga",
    "LeBorg James",
    "The Good, The Borg, and The Ugly",
    "Borg Bunny",
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
    "Borgatti",
    "Borgleone",
    "Neil Armborg",
    "Amelia Borghart",
    "Marie Borgie",
    "Oppenborger",
    "Borgdwin",
    "Borgsworth",
    "Van Borghen",
    "Remborgdt",
    "Strasborg",
    "Borgdeaux",
    "Borgona",
    "Borggundy",
    "Borgcelona",
    "Borgamo",
    "Borgpool",
    "Borg Vader",
    "Frodo Borggins",
    "Borglock Holmes",
    "James Borgd",
    "Indiana Borgs",
    "Captain Borgmerica",
    "The Incredible Borg",
    "Borg to the Future",
    "Jurassic Borg",
    "The Borgfather",
    "Borgbusters",
    "Borg Wars",
    "The Borgrix",
    "Doctor Borgenstein",
    "Frankenborg",
    "Borgalicious",
    "Borgmeister",
    "Borgnado",
    "Borgtastic",
    "Borgpocalypse",
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
    Channels,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Welcome,
        Tab::Security,
        Tab::Provider,
        Tab::ApiKey,
        Tab::Channels,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Welcome => "Welcome",
            Tab::Security => "Disclaimer",
            Tab::Provider => "Provider",
            Tab::ApiKey => "API Key",
            Tab::Channels => "Channels",
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

/// A channel plugin item for the onboarding channels picker.
struct ChannelItem {
    id: String,
    name: String,
    selected: bool,
    cred_specs: &'static [borg_plugins::CredentialSpec],
    /// Collected credentials once the user has entered them.
    credentials: Vec<(String, String)>,
    platform_available: bool,
}

/// Phase of the channels picker.
#[derive(Clone)]
enum ChannelPhase {
    Browsing,
    CredentialInput {
        /// Index into `channel_items` being configured.
        item_idx: usize,
        /// Which credential within the item we're prompting for.
        cred_idx: usize,
        /// Current text input buffer.
        buffer: String,
        /// Credentials collected so far for this item.
        collected: Vec<(String, String)>,
    },
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

    // API Key
    api_key_input: String,
    api_key_existing: bool,

    // Channels
    channel_items: Vec<ChannelItem>,
    channel_cursor: usize,
    channel_phase: ChannelPhase,

    // Model selection submenu (between ApiKey/Provider and Channels)
    selecting_model: bool,
    model_index: usize,

    // Validation hints
    api_key_required_hint: bool,

    /// Highest tab index the user has reached (for tab-bar checkmarks).
    furthest_tab: usize,

    // Result
    done: bool,
    cancelled: bool,
}

impl OnboardingState {
    fn new() -> Self {
        let api_key_existing = PROVIDERS
            .iter()
            .any(|(id, _, _)| Self::check_existing_api_key_cached(id));

        let default_name = random_borg_name();

        // Load channel plugins from catalog
        let channel_items: Vec<ChannelItem> = borg_plugins::catalog::CATALOG
            .iter()
            .filter(|def| def.kind == borg_plugins::PluginKind::Channel)
            .map(|def| ChannelItem {
                id: def.id.to_string(),
                name: def.name.to_string(),
                selected: false,
                cred_specs: def.required_credentials,
                credentials: Vec::new(),
                platform_available: def.platform.is_available(),
            })
            .collect();

        Self {
            tab: Tab::Welcome,
            input_mode: InputMode::TextInput,
            user_name: String::new(),
            agent_name: default_name.clone(),
            default_agent_name: default_name,
            welcome_focus: WelcomeFocus::UserName,
            security_accepted: false,
            provider_index: 0,
            api_key_input: String::new(),
            api_key_existing,
            channel_items,
            channel_cursor: 0,
            channel_phase: ChannelPhase::Browsing,
            selecting_model: false,
            model_index: 0,
            api_key_required_hint: false,
            furthest_tab: 0,
            done: false,
            cancelled: false,
        }
    }

    fn check_existing_api_key_cached(provider_id: &str) -> bool {
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

    fn tab_completed(&self, tab: Tab) -> bool {
        // Only show a checkmark if the user has visited this tab
        if tab.index() > self.furthest_tab {
            return false;
        }
        match tab {
            Tab::Welcome => !self.user_name.is_empty(),
            Tab::Security => self.security_accepted,
            Tab::Provider | Tab::Channels => true,
            Tab::ApiKey => false,
        }
    }

    /// Whether the selected provider needs an API key.
    fn selected_provider_needs_key(&self) -> bool {
        let (id, _, _) = PROVIDERS[self.provider_index];
        id != "ollama"
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
        let model_id = models
            .get(self.model_index)
            .unwrap_or(&models[0])
            .0
            .to_string();

        let channels: Vec<(String, Vec<(String, String)>)> = self
            .channel_items
            .iter()
            .filter(|item| item.selected)
            .map(|item| (item.id.clone(), item.credentials.clone()))
            .collect();

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
            channels,
        }
    }

    fn next_tab(&mut self) {
        let idx = self.tab.index();
        if idx < Tab::ALL.len() - 1 {
            match self.tab {
                Tab::Welcome if self.user_name.trim().is_empty() => return,
                Tab::Security if !self.security_accepted => return,
                Tab::Provider => {
                    // Always go to model selection after provider
                    self.activate_model_selection();
                    return;
                }
                Tab::ApiKey => {
                    // Go directly to Channels after API key
                    self.tab = Tab::Channels;
                    self.update_input_mode();
                    return;
                }
                Tab::Channels => {
                    self.finish_channels();
                    return;
                }
                _ => {}
            }
            self.tab = Tab::ALL[idx + 1];
            self.update_input_mode();
        }
    }

    fn prev_tab(&mut self) {
        // From Channels, go back to ApiKey (if needed) or model selection
        if self.tab == Tab::Channels {
            if self.selected_provider_needs_key() {
                self.tab = Tab::ApiKey;
                self.update_input_mode();
            } else {
                self.selecting_model = true;
                self.input_mode = InputMode::Normal;
            }
            return;
        }
        // From ApiKey, go back to model selection submenu
        if self.tab == Tab::ApiKey {
            self.selecting_model = true;
            self.input_mode = InputMode::Normal;
            return;
        }
        let idx = self.tab.index();
        if idx > 0 {
            self.tab = Tab::ALL[idx - 1];
            self.update_input_mode();
        }
    }

    fn update_input_mode(&mut self) {
        if self.selecting_model {
            self.input_mode = InputMode::Normal;
            return;
        }
        self.input_mode = match self.tab {
            Tab::Welcome => InputMode::TextInput,
            Tab::ApiKey if !self.api_key_existing => InputMode::TextInput,
            Tab::Channels => match self.channel_phase {
                ChannelPhase::Browsing => InputMode::Normal,
                ChannelPhase::CredentialInput { .. } => InputMode::TextInput,
            },
            Tab::Provider | Tab::Security => InputMode::Normal,
            _ => InputMode::Normal,
        };
    }

    fn handle_paste(&mut self, text: &str) {
        match self.tab {
            Tab::Welcome => match self.welcome_focus {
                WelcomeFocus::UserName => self.user_name.push_str(text),
                WelcomeFocus::AgentName => self.agent_name.push_str(text),
            },
            Tab::ApiKey if !self.api_key_existing => {
                self.api_key_input.push_str(text);
                self.api_key_required_hint = false;
            }
            Tab::Channels => {
                if let ChannelPhase::CredentialInput { ref mut buffer, .. } = self.channel_phase {
                    buffer.push_str(text);
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Esc in credential input cancels back to browsing, not the whole wizard
        if key.code == KeyCode::Esc {
            if self.selecting_model {
                self.cancel_model_selection();
                return;
            }
            if self.tab == Tab::Channels {
                match &self.channel_phase {
                    ChannelPhase::CredentialInput { item_idx, .. } => {
                        // Cancel credential entry — deselect the item
                        self.channel_items[*item_idx].selected = false;
                        self.channel_phase = ChannelPhase::Browsing;
                        self.update_input_mode();
                        return;
                    }
                    ChannelPhase::Browsing => {
                        // Esc from browsing finishes (channels are optional)
                        self.finish_channels();
                        return;
                    }
                }
            }
            self.cancelled = true;
            return;
        }

        if self.selecting_model {
            self.handle_model_selection_key(key);
            return;
        }

        match self.tab {
            Tab::Welcome => self.handle_welcome_key(key),
            Tab::Security => self.handle_security_key(key),
            Tab::Provider => self.handle_provider_key(key),
            Tab::ApiKey => self.handle_api_key_key(key),
            Tab::Channels => self.handle_channels_key(key),
        }

        // Track the furthest tab the user has reached (for tab-bar checkmarks)
        let idx = self.tab.index();
        if idx > self.furthest_tab {
            self.furthest_tab = idx;
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

    fn activate_model_selection(&mut self) {
        self.model_index = 0;
        self.selecting_model = true;
        self.input_mode = InputMode::Normal;
    }

    fn handle_model_selection_key(&mut self, key: KeyEvent) {
        let (provider_id, _, _) = PROVIDERS[self.provider_index];
        let models = models_for_provider(provider_id);
        let model_count = models.len();

        match key.code {
            KeyCode::Up => {
                if self.model_index > 0 {
                    self.model_index -= 1;
                } else {
                    self.model_index = model_count - 1;
                }
            }
            KeyCode::Down => {
                self.model_index = (self.model_index + 1) % model_count;
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => {
                if is_shift_tab(&key) {
                    self.cancel_model_selection();
                } else {
                    self.confirm_model_selection();
                }
            }
            KeyCode::Left => {
                self.cancel_model_selection();
            }
            _ => {}
        }
    }

    fn confirm_model_selection(&mut self) {
        self.selecting_model = false;
        if self.selected_provider_needs_key() {
            self.tab = Tab::ApiKey;
        } else {
            self.tab = Tab::Channels;
        }
        self.update_input_mode();
    }

    fn cancel_model_selection(&mut self) {
        self.selecting_model = false;
        self.tab = Tab::Provider;
        self.update_input_mode();
    }

    fn handle_provider_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.provider_index > 0 {
                    self.provider_index -= 1;
                } else {
                    self.provider_index = PROVIDERS.len() - 1;
                }
                self.model_index = 0;
            }
            KeyCode::Down => {
                self.provider_index = (self.provider_index + 1) % PROVIDERS.len();
                self.model_index = 0;
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
                        self.next_tab();
                    }
                }
                KeyCode::Left => self.prev_tab(),
                KeyCode::Enter => {
                    self.next_tab();
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
                    self.next_tab();
                }
            }
            KeyCode::Right => {
                if self.api_key_input.trim().is_empty() {
                    self.api_key_required_hint = true;
                } else {
                    self.api_key_required_hint = false;
                    self.next_tab();
                }
            }
            KeyCode::Left => self.prev_tab(),
            KeyCode::Enter => {
                if self.api_key_input.trim().is_empty() {
                    self.api_key_required_hint = true;
                } else {
                    self.api_key_required_hint = false;
                    self.next_tab();
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

    fn handle_channels_key(&mut self, key: KeyEvent) {
        match &self.channel_phase.clone() {
            ChannelPhase::Browsing => match key.code {
                KeyCode::Up => {
                    if !self.channel_items.is_empty() {
                        if self.channel_cursor == 0 {
                            self.channel_cursor = self.channel_items.len() - 1;
                        } else {
                            self.channel_cursor -= 1;
                        }
                    }
                }
                KeyCode::Down => {
                    if !self.channel_items.is_empty() {
                        self.channel_cursor = (self.channel_cursor + 1) % self.channel_items.len();
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = self.channel_items.get_mut(self.channel_cursor) {
                        if !item.platform_available {
                            return;
                        }
                        if item.selected {
                            // Deselect
                            item.selected = false;
                            item.credentials.clear();
                        } else {
                            // Select — if credentials needed, start input
                            let has_required = item.cred_specs.iter().any(|c| !c.is_optional);
                            if has_required {
                                item.selected = true;
                                self.channel_phase = ChannelPhase::CredentialInput {
                                    item_idx: self.channel_cursor,
                                    cred_idx: 0,
                                    buffer: String::new(),
                                    collected: Vec::new(),
                                };
                                self.update_input_mode();
                            } else {
                                item.selected = true;
                            }
                        }
                    }
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if is_shift_tab(&key) {
                        self.prev_tab();
                    } else {
                        self.finish_channels();
                    }
                }
                KeyCode::Left => self.prev_tab(),
                _ => {}
            },
            ChannelPhase::CredentialInput {
                item_idx,
                cred_idx,
                buffer,
                collected,
            } => {
                let item_idx = *item_idx;
                let cred_idx = *cred_idx;
                let buffer = buffer.clone();
                let collected = collected.clone();

                match key.code {
                    KeyCode::Enter => {
                        let value = buffer.trim().to_string();
                        let spec = &self.channel_items[item_idx].cred_specs[cred_idx];
                        // Required credentials must be non-empty; optional ones can be skipped
                        if value.is_empty() && !spec.is_optional {
                            return;
                        }
                        let mut new_collected = collected;
                        if !value.is_empty() {
                            new_collected.push((spec.key.to_string(), value));
                        }

                        let total_creds = self.channel_items[item_idx].cred_specs.len();
                        if cred_idx + 1 < total_creds {
                            // More credentials to collect
                            self.channel_phase = ChannelPhase::CredentialInput {
                                item_idx,
                                cred_idx: cred_idx + 1,
                                buffer: String::new(),
                                collected: new_collected,
                            };
                        } else {
                            // All credentials collected
                            self.channel_items[item_idx].credentials = new_collected;
                            self.channel_phase = ChannelPhase::Browsing;
                            self.update_input_mode();
                        }
                    }
                    KeyCode::Tab if is_shift_tab(&key) => {
                        // Cancel credential entry and go back to previous tab
                        self.channel_items[item_idx].selected = false;
                        self.channel_phase = ChannelPhase::Browsing;
                        self.update_input_mode();
                        self.prev_tab();
                    }
                    KeyCode::Left => {
                        // Cancel credential entry and go back to previous tab
                        self.channel_items[item_idx].selected = false;
                        self.channel_phase = ChannelPhase::Browsing;
                        self.update_input_mode();
                        self.prev_tab();
                    }
                    KeyCode::Backspace => {
                        if let ChannelPhase::CredentialInput { ref mut buffer, .. } =
                            self.channel_phase
                        {
                            buffer.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let ChannelPhase::CredentialInput { ref mut buffer, .. } =
                            self.channel_phase
                        {
                            buffer.push(c);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Finish the channels tab — start credential collection or complete.
    fn finish_channels(&mut self) {
        // Check if any selected channels still need credentials
        let needs_creds = self.channel_items.iter().enumerate().find(|(_, item)| {
            item.selected
                && item.credentials.is_empty()
                && item.cred_specs.iter().any(|c| !c.is_optional)
        });

        if let Some((idx, _)) = needs_creds {
            self.channel_phase = ChannelPhase::CredentialInput {
                item_idx: idx,
                cred_idx: 0,
                buffer: String::new(),
                collected: Vec::new(),
            };
            self.update_input_mode();
        } else {
            self.done = true;
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

    if state.selecting_model {
        render_model_selection(frame, content_area, state);
        return;
    }

    match state.tab {
        Tab::Welcome => render_welcome(frame, content_area, state),
        Tab::Security => render_security(frame, content_area, state),
        Tab::Provider => render_provider(frame, content_area, state),
        Tab::ApiKey => render_api_key(frame, content_area, state),
        Tab::Channels => render_channels(frame, content_area, state),
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

    for (i, (_id, label, desc)) in PROVIDERS.iter().enumerate() {
        let is_selected = i == state.provider_index;
        let indicator = if is_selected { "● " } else { "○ " };
        let style = if is_selected {
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("  {indicator}{label}"), style),
            Span::styled(
                format!("  {desc}"),
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

fn render_model_selection(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let (provider_id, provider_label, _) = PROVIDERS[state.provider_index];
    let models = models_for_provider(provider_id);

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("Choose a model for {provider_label}"),
        Style::default().fg(theme::CYAN),
    )));
    lines.push(Line::default());

    for (i, (_id, label)) in models.iter().enumerate() {
        let is_selected = i == state.model_index;
        let indicator = if is_selected { "● " } else { "○ " };
        let style = if is_selected {
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };

        lines.push(Line::from(Span::styled(
            format!("  {indicator}{label}"),
            style,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_channels(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let mut lines: Vec<Line> = Vec::new();

    match &state.channel_phase {
        ChannelPhase::Browsing => {
            lines.push(Line::from(Span::styled(
                "Connect messaging channels (optional)",
                Style::default().fg(theme::CYAN),
            )));
            lines.push(Line::default());

            for (i, item) in state.channel_items.iter().enumerate() {
                let is_selected = i == state.channel_cursor;
                let check = if item.selected { "[x]" } else { "[ ]" };

                let style = if !item.platform_available {
                    theme::dim()
                } else if is_selected {
                    Style::default()
                        .fg(theme::CYAN)
                        .add_modifier(Modifier::BOLD)
                } else if item.selected {
                    Style::default().fg(theme::GREEN)
                } else {
                    theme::dim()
                };

                let mut spans = vec![Span::styled(format!("  {check} {}", item.name), style)];

                if !item.platform_available {
                    spans.push(Span::styled("  (macOS only)", theme::dim()));
                } else if item.selected && !item.credentials.is_empty() {
                    spans.push(Span::styled("  ✓", Style::default().fg(theme::GREEN)));
                }

                lines.push(Line::from(spans));
            }

            if state.channel_items.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  No channel plugins available",
                    theme::dim(),
                )));
            }

            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Press Enter to finish, or Space to toggle channels",
                theme::dim(),
            )));
        }
        ChannelPhase::CredentialInput {
            item_idx,
            cred_idx,
            buffer,
            ..
        } => {
            let item = &state.channel_items[*item_idx];
            let spec = &item.cred_specs[*cred_idx];

            lines.push(Line::from(Span::styled(
                format!("Configure {}", item.name),
                Style::default().fg(theme::CYAN),
            )));
            lines.push(Line::default());

            lines.push(Line::from(Span::styled(
                format!(
                    "  {} ({}/{})",
                    spec.label,
                    cred_idx + 1,
                    item.cred_specs.len()
                ),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            )));

            if !spec.help_url.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", spec.help_url),
                    theme::dim(),
                )));
            }
            lines.push(Line::default());

            let masked: String = "*".repeat(buffer.len());
            lines.push(Line::from(vec![
                Span::styled("  › ", Style::default().fg(theme::CYAN)),
                Span::raw(masked),
                Span::styled("▊", Style::default().fg(theme::CYAN)),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let hint = if state.selecting_model {
        " ↑/↓: select  Enter/Tab: next  Shift+Tab/←: back  Esc: back"
    } else {
        match state.tab {
            Tab::Welcome => " Tab: next field  Enter: next  Esc: cancel",
            Tab::Provider => " ↑/↓: select  Enter/Tab: next  Shift+Tab: back  Esc: cancel",
            Tab::Channels => match state.channel_phase {
                ChannelPhase::Browsing => " ↑/↓: select  Space: toggle  Enter: finish  Esc: skip",
                ChannelPhase::CredentialInput { .. } => {
                    " Type to enter  Enter: submit  Esc: cancel"
                }
            },
            _ if state.input_mode == InputMode::TextInput => {
                " Type to enter  Tab: next  Esc: cancel"
            }
            _ => " Tab/→: next  Shift+Tab/←: back  Esc: cancel",
        }
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
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    state.handle_key(key);
                }
                Event::Paste(text) => {
                    state.handle_paste(&text);
                }
                _ => {}
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
            [
                Tab::Welcome,
                Tab::Security,
                Tab::Provider,
                Tab::ApiKey,
                Tab::Channels,
            ]
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
        // Model selection submenu activates after provider
        assert!(state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);
        state.confirm_model_selection();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::ApiKey);
        state.next_tab();
        assert_eq!(state.tab, Tab::Channels);
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
        // Simulate having visited all tabs
        state.furthest_tab = Tab::Channels.index();
        assert!(!state.tab_completed(Tab::Welcome)); // empty name
        state.user_name = "Test".to_string();
        assert!(state.tab_completed(Tab::Welcome));
        assert!(!state.tab_completed(Tab::Security)); // not accepted
        state.security_accepted = true;
        assert!(state.tab_completed(Tab::Security));
        assert!(!state.tab_completed(Tab::ApiKey));
        assert!(state.tab_completed(Tab::Channels)); // optional, always true
    }

    #[test]
    fn tab_completed_requires_visited() {
        let state = OnboardingState::new();
        // On a fresh install, no tabs should show as completed
        assert!(!state.tab_completed(Tab::Provider));
        assert!(!state.tab_completed(Tab::Channels));
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

        // Find ollama index (keyless provider)
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;

        // next_tab from Provider always goes to model selection submenu
        state.next_tab();
        assert!(state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);
        assert!(!state.done);
    }

    #[test]
    fn api_key_provider_goes_to_model_selection() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        state.tab = Tab::Provider;
        state.provider_index = 0; // OpenRouter needs API key

        // Provider always goes to model selection first
        state.next_tab();
        assert!(state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);
        assert!(!state.done);
    }

    #[test]
    fn selected_provider_needs_key_logic() {
        let mut state = OnboardingState::new();

        // OpenRouter needs key
        state.provider_index = 0;
        assert!(state.selected_provider_needs_key());

        // Ollama doesn't need key
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        assert!(!state.selected_provider_needs_key());
    }

    #[test]
    fn provider_tab_completed_after_visited() {
        let mut state = OnboardingState::new();
        // Not completed until user has reached it
        assert!(!state.tab_completed(Tab::Provider));
        state.furthest_tab = Tab::Provider.index();
        assert!(state.tab_completed(Tab::Provider));
    }

    #[test]
    fn channels_tab_enter_with_no_selection_finishes() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Channels;
        // Enter with no channels selected should finish
        state.finish_channels();
        assert!(state.done);
    }

    #[test]
    fn channels_tab_loads_catalog_items() {
        let state = OnboardingState::new();
        // Should have loaded channel plugins from CATALOG
        assert!(
            !state.channel_items.is_empty(),
            "Expected channel items from catalog"
        );
        // All items should be channels
        for item in &state.channel_items {
            assert!(
                item.id.starts_with("messaging/"),
                "Expected messaging plugin, got: {}",
                item.id
            );
        }
    }

    // ── Model selection submenu tests ──

    #[test]
    fn model_submenu_activates_after_provider() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        state.tab = Tab::Provider;
        state.provider_index = 0; // OpenRouter
        state.next_tab();
        assert!(state.selecting_model);
        assert_eq!(state.model_index, 0);
    }

    #[test]
    fn model_submenu_activates_for_keyless_provider() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.security_accepted = true;
        state.tab = Tab::Provider;
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        state.next_tab();
        assert!(state.selecting_model);
    }

    #[test]
    fn model_confirm_advances_to_api_key_when_needed() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        state.provider_index = 0; // OpenRouter (needs key)
        state.confirm_model_selection();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::ApiKey);
    }

    #[test]
    fn model_confirm_advances_to_channels_for_keyless() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        state.confirm_model_selection();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::Channels);
    }

    #[test]
    fn model_cancel_always_returns_to_provider() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        state.provider_index = 0; // OpenRouter (needs key)
        state.cancel_model_selection();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);

        // Same for keyless provider
        state.selecting_model = true;
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        state.cancel_model_selection();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);
    }

    #[test]
    fn model_selection_up_down_navigation() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        state.provider_index = 0; // OpenRouter
        let model_count = models_for_provider("openrouter").len();

        // Down increments
        state.handle_model_selection_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.model_index, 1);

        // Up decrements
        state.handle_model_selection_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.model_index, 0);

        // Up wraps to last
        state.handle_model_selection_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.model_index, model_count - 1);

        // Down wraps to first
        state.handle_model_selection_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.model_index, 0);
    }

    #[test]
    fn model_index_resets_on_provider_change() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Provider;
        state.provider_index = 0;
        state.model_index = 3;

        state.handle_provider_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.model_index, 0);
    }

    #[test]
    fn build_result_uses_model_index() {
        let mut state = OnboardingState::new();
        state.user_name = "Test".to_string();
        state.provider_index = 0; // OpenRouter
        state.model_index = 2;
        let result = state.build_result();
        let models = models_for_provider("openrouter");
        assert_eq!(result.model_id, models[2].0);
    }

    #[test]
    fn model_selection_enter_confirms() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        state.provider_index = 0; // OpenRouter (needs key)
        state.model_index = 1;
        state.handle_model_selection_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::ApiKey);
    }

    #[test]
    fn model_selection_esc_cancels() {
        let mut state = OnboardingState::new();
        state.selecting_model = true;
        state.provider_index = 0;
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::Provider);
        assert!(!state.cancelled);
    }

    #[test]
    fn back_from_channels_goes_to_api_key_when_needed() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Channels;
        state.provider_index = 0; // OpenRouter (needs key)
        state.prev_tab();
        assert!(!state.selecting_model);
        assert_eq!(state.tab, Tab::ApiKey);
    }

    #[test]
    fn back_from_channels_enters_model_submenu_for_keyless() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Channels;
        let ollama_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "ollama")
            .unwrap();
        state.provider_index = ollama_idx;
        state.prev_tab();
        assert!(state.selecting_model);
    }

    #[test]
    fn back_from_api_key_enters_model_submenu() {
        let mut state = OnboardingState::new();
        state.tab = Tab::ApiKey;
        state.prev_tab();
        assert!(state.selecting_model);
    }

    #[test]
    fn channels_tab_space_toggles() {
        let mut state = OnboardingState::new();
        state.tab = Tab::Channels;
        state.channel_cursor = 0;

        // Find a channel that doesn't need credentials (no required creds)
        let no_cred_idx = state.channel_items.iter().position(|item| {
            item.platform_available && !item.cred_specs.iter().any(|c| !c.is_optional)
        });

        if let Some(idx) = no_cred_idx {
            state.channel_cursor = idx;
            assert!(!state.channel_items[idx].selected);
            // Space toggles on
            state.handle_channels_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            assert!(state.channel_items[idx].selected);
            // Space toggles off
            state.handle_channels_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            assert!(!state.channel_items[idx].selected);
        }
    }
}
