use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::db::Database;
use borg_core::settings::SettingSource;

use crate::onboarding::{models_for_provider, PROVIDERS};

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SettingKind {
    Bool,
    Text,
    Float,
    Uint,
    Select,
}

#[derive(Clone, Copy)]
pub struct SettingEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: SettingKind,
    pub category: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditMode {
    Browsing,
    Editing {
        buffer: String,
    },
    ConfirmReset,
    SelectingModel {
        selected: usize,
        scroll_offset: usize,
    },
}

pub struct SettingsPopup {
    visible: bool,
    entries: &'static [SettingEntry],
    selected: usize,
    mode: EditMode,
    status_message: Option<(String, bool)>, // (message, is_success)
    db: Option<Database>,
    provider_index: usize,
    model_index: usize,
}

const SETTINGS: &[SettingEntry] = &[
    // — LLM —
    SettingEntry {
        key: "provider",
        label: "Provider",
        kind: SettingKind::Select,
        category: "LLM",
    },
    SettingEntry {
        key: "model",
        label: "Model",
        kind: SettingKind::Select,
        category: "LLM",
    },
    SettingEntry {
        key: "temperature",
        label: "Temperature",
        kind: SettingKind::Float,
        category: "LLM",
    },
    SettingEntry {
        key: "max_tokens",
        label: "Response length",
        kind: SettingKind::Uint,
        category: "LLM",
    },
    // — Conversation —
    SettingEntry {
        key: "conversation.max_iterations",
        label: "Max agent steps",
        kind: SettingKind::Uint,
        category: "Conversation",
    },
    SettingEntry {
        key: "conversation.show_thinking",
        label: "Show reasoning",
        kind: SettingKind::Bool,
        category: "Conversation",
    },
    SettingEntry {
        key: "conversation.collaboration_mode",
        label: "Mode",
        kind: SettingKind::Select,
        category: "Conversation",
    },
    // — Security —
    SettingEntry {
        key: "sandbox.enabled",
        label: "Sandbox",
        kind: SettingKind::Bool,
        category: "Security",
    },
    SettingEntry {
        key: "skills.enabled",
        label: "Allow skills",
        kind: SettingKind::Bool,
        category: "Security",
    },
    SettingEntry {
        key: "security.secret_detection",
        label: "Secret detection",
        kind: SettingKind::Bool,
        category: "Security",
    },
    // — Budget —
    SettingEntry {
        key: "budget.monthly_token_limit",
        label: "Monthly limit",
        kind: SettingKind::Uint,
        category: "Budget",
    },
    SettingEntry {
        key: "budget.warning_threshold",
        label: "Budget warning",
        kind: SettingKind::Float,
        category: "Budget",
    },
    // — Voice —
    SettingEntry {
        key: "tts.enabled",
        label: "Enabled",
        kind: SettingKind::Bool,
        category: "Voice",
    },
    SettingEntry {
        key: "tts.auto_mode",
        label: "Auto reply",
        kind: SettingKind::Bool,
        category: "Voice",
    },
    // — Evolution —
    SettingEntry {
        key: "evolution.enabled",
        label: "Evolution",
        kind: SettingKind::Bool,
        category: "Evolution",
    },
    // — Workflow —
    SettingEntry {
        key: "workflow.enabled",
        label: "Workflows",
        kind: SettingKind::Select,
        category: "Conversation",
    },
];

impl SettingsPopup {
    pub fn new() -> Self {
        let db = match Database::open() {
            Ok(db) => Some(db),
            Err(e) => {
                tracing::warn!("Settings popup: failed to open database: {e}");
                None
            }
        };
        Self {
            visible: false,
            entries: SETTINGS,
            selected: 0,
            mode: EditMode::Browsing,
            status_message: None,
            db,
            provider_index: 0,
            model_index: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self, config: &Config) {
        self.visible = true;
        self.selected = 0;
        self.mode = EditMode::Browsing;
        self.status_message = None;
        self.sync_select_indices(config);
    }

    /// Sync provider_index and model_index from the current config values.
    fn sync_select_indices(&mut self, config: &Config) {
        let provider_id = config.llm.provider.as_deref().unwrap_or("openrouter");
        self.provider_index = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == provider_id)
            .unwrap_or(0);

        let models = models_for_provider(provider_id);
        self.model_index = models
            .iter()
            .position(|(id, _)| *id == config.llm.model.as_str())
            .unwrap_or(0);
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.mode = EditMode::Browsing;
        self.status_message = None;
    }

    fn current_value(&self, config: &Config, key: &str) -> String {
        match key {
            "provider" => {
                let (_, display, _) = PROVIDERS.get(self.provider_index).unwrap_or(&PROVIDERS[0]);
                display.to_string()
            }
            "model" => {
                let provider_id = PROVIDERS
                    .get(self.provider_index)
                    .map(|(id, _, _)| *id)
                    .unwrap_or("openrouter");
                let models = models_for_provider(provider_id);
                models
                    .get(self.model_index)
                    .map(|(_, display)| display.to_string())
                    .unwrap_or_else(|| config.llm.model.clone())
            }
            _ => borg_core::settings::config_value_for_key(config, key)
                .unwrap_or_else(|| "?".to_string()),
        }
    }

    /// Handle a bracketed paste event. Returns `true` if the paste was consumed
    /// (i.e. the popup is visible and in text-editing mode).
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.visible {
            return false;
        }
        if let EditMode::Editing { ref mut buffer } = self.mode {
            buffer.push_str(text);
            return true;
        }
        false
    }

    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> anyhow::Result<Option<AppAction>> {
        use crossterm::event::KeyCode;

        match &mut self.mode {
            EditMode::Browsing => match key.code {
                KeyCode::Esc => {
                    self.dismiss();
                    Ok(None)
                }
                KeyCode::Up => {
                    if self.selected == 0 {
                        self.selected = self.entries.len() - 1;
                    } else {
                        self.selected -= 1;
                    }
                    self.status_message = None;
                    Ok(None)
                }
                KeyCode::Down => {
                    self.selected = (self.selected + 1) % self.entries.len();
                    self.status_message = None;
                    Ok(None)
                }
                KeyCode::Char(' ') => {
                    let entry = &self.entries[self.selected];
                    if entry.kind == SettingKind::Bool {
                        return self.toggle_bool(config);
                    }
                    Ok(None)
                }
                KeyCode::Left => {
                    let entry = &self.entries[self.selected];
                    if entry.key == "model" {
                        return self.open_model_selection();
                    }
                    match entry.kind {
                        SettingKind::Select => self.cycle_select(config, false),
                        SettingKind::Float => self.step_float(config, false),
                        _ => Ok(None),
                    }
                }
                KeyCode::Right => {
                    let entry = &self.entries[self.selected];
                    if entry.key == "model" {
                        return self.open_model_selection();
                    }
                    match entry.kind {
                        SettingKind::Select => self.cycle_select(config, true),
                        SettingKind::Float => self.step_float(config, true),
                        _ => Ok(None),
                    }
                }
                KeyCode::Enter => {
                    let entry = &self.entries[self.selected];
                    if entry.key == "model" {
                        return self.open_model_selection();
                    }
                    match entry.kind {
                        SettingKind::Bool | SettingKind::Select => {
                            self.dismiss();
                            Ok(None)
                        }
                        _ => {
                            let current = self.current_value(config, entry.key);
                            self.mode = EditMode::Editing { buffer: current };
                            self.status_message = None;
                            Ok(None)
                        }
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.mode = EditMode::ConfirmReset;
                    self.status_message = None;
                    Ok(None)
                }
                _ => Ok(None),
            },
            EditMode::ConfirmReset => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let result = self.reset_all_to_defaults(config);
                    self.mode = EditMode::Browsing;
                    result
                }
                _ => {
                    self.mode = EditMode::Browsing;
                    self.status_message = None;
                    Ok(None)
                }
            },
            EditMode::Editing { buffer } => match key.code {
                KeyCode::Esc => {
                    self.mode = EditMode::Browsing;
                    self.status_message = None;
                    Ok(None)
                }
                KeyCode::Enter => {
                    let value = buffer.clone();
                    let entry = &self.entries[self.selected];
                    let key_str = entry.key;
                    match config.apply_setting(key_str, &value) {
                        Ok(confirmation) => {
                            if let Err(e) = self.save_setting(key_str, &value) {
                                self.status_message = Some((format!("Save failed: {e}"), false));
                                self.mode = EditMode::Browsing;
                                return Ok(None);
                            }
                            self.status_message = Some((format!("Updated: {confirmation}"), true));
                            self.mode = EditMode::Browsing;
                            Ok(Some(AppAction::UpdateSetting {
                                key: key_str.to_string(),
                                value,
                            }))
                        }
                        Err(e) => {
                            self.status_message = Some((format!("Error: {e}"), false));
                            self.mode = EditMode::Browsing;
                            Ok(None)
                        }
                    }
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    Ok(None)
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    Ok(None)
                }
                _ => Ok(None),
            },
            EditMode::SelectingModel {
                ref mut selected,
                ref mut scroll_offset,
            } => match key.code {
                KeyCode::Up => {
                    let provider_id = PROVIDERS
                        .get(self.provider_index)
                        .map(|(id, _, _)| *id)
                        .unwrap_or("openrouter");
                    let count = models_for_provider(provider_id).len();
                    *selected = if *selected == 0 {
                        count - 1
                    } else {
                        *selected - 1
                    };
                    // Adjust scroll
                    if *selected < *scroll_offset {
                        *scroll_offset = *selected;
                    }
                    Ok(None)
                }
                KeyCode::Down => {
                    let provider_id = PROVIDERS
                        .get(self.provider_index)
                        .map(|(id, _, _)| *id)
                        .unwrap_or("openrouter");
                    let count = models_for_provider(provider_id).len();
                    *selected = (*selected + 1) % count;
                    Ok(None)
                }
                KeyCode::Enter => {
                    let sel = *selected;
                    let provider_id = PROVIDERS
                        .get(self.provider_index)
                        .map(|(id, _, _)| *id)
                        .unwrap_or("openrouter");
                    let models = models_for_provider(provider_id);
                    if let Some((model_id, _)) = models.get(sel) {
                        match config.apply_setting("model", model_id) {
                            Ok(confirmation) => {
                                if let Err(e) = self.save_setting("model", model_id) {
                                    self.status_message =
                                        Some((format!("Save failed: {e}"), false));
                                    self.mode = EditMode::Browsing;
                                    return Ok(None);
                                }
                                self.model_index = sel;
                                self.status_message =
                                    Some((format!("Updated: {confirmation}"), true));
                                self.mode = EditMode::Browsing;
                                return Ok(Some(AppAction::UpdateSetting {
                                    key: "model".to_string(),
                                    value: model_id.to_string(),
                                }));
                            }
                            Err(e) => {
                                self.status_message = Some((format!("Error: {e}"), false));
                                self.mode = EditMode::Browsing;
                            }
                        }
                    }
                    Ok(None)
                }
                KeyCode::Esc => {
                    self.mode = EditMode::Browsing;
                    self.status_message = None;
                    Ok(None)
                }
                _ => Ok(None),
            },
        }
    }

    /// Open the model selection list, pre-selecting the current model.
    fn open_model_selection(&mut self) -> anyhow::Result<Option<AppAction>> {
        self.mode = EditMode::SelectingModel {
            selected: self.model_index,
            scroll_offset: 0,
        };
        self.status_message = None;
        Ok(None)
    }

    fn cycle_select(
        &mut self,
        config: &mut Config,
        forward: bool,
    ) -> anyhow::Result<Option<AppAction>> {
        let entry = &self.entries[self.selected];
        let mut actions: Vec<AppAction> = Vec::new();

        match entry.key {
            "provider" => {
                let count = PROVIDERS.len();
                self.provider_index = if forward {
                    (self.provider_index + 1) % count
                } else {
                    (self.provider_index + count - 1) % count
                };
                let (id, _, _) = PROVIDERS[self.provider_index];
                match config.apply_setting("provider", id) {
                    Ok(confirmation) => {
                        if let Err(e) = self.save_setting("provider", id) {
                            self.status_message = Some((format!("Save failed: {e}"), false));
                            return Ok(None);
                        }
                        self.status_message = Some((format!("Updated: {confirmation}"), true));
                        actions.push(AppAction::UpdateSetting {
                            key: "provider".to_string(),
                            value: id.to_string(),
                        });
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Error: {e}"), false));
                        return Ok(None);
                    }
                }
                // Reset model to first option for new provider
                self.model_index = 0;
                let models = models_for_provider(id);
                if let Some((model_id, _)) = models.first() {
                    if config.apply_setting("model", model_id).is_ok() {
                        if let Err(e) = self.save_setting("model", model_id) {
                            tracing::warn!("Failed to persist model reset: {e}");
                        }
                        actions.push(AppAction::UpdateSetting {
                            key: "model".to_string(),
                            value: model_id.to_string(),
                        });
                    }
                }
            }
            "conversation.collaboration_mode" => {
                const MODES: &[&str] = &["default", "execute", "plan"];
                let current = format!("{}", config.conversation.collaboration_mode);
                let idx = MODES.iter().position(|&m| m == current).unwrap_or(0);
                let next_idx = if forward {
                    (idx + 1) % MODES.len()
                } else {
                    (idx + MODES.len() - 1) % MODES.len()
                };
                let new_mode = MODES[next_idx];
                match config.apply_setting("conversation.collaboration_mode", new_mode) {
                    Ok(confirmation) => {
                        if let Err(e) =
                            self.save_setting("conversation.collaboration_mode", new_mode)
                        {
                            self.status_message = Some((format!("Save failed: {e}"), false));
                            return Ok(None);
                        }
                        self.status_message = Some((format!("Updated: {confirmation}"), true));
                        actions.push(AppAction::UpdateSetting {
                            key: "conversation.collaboration_mode".to_string(),
                            value: new_mode.to_string(),
                        });
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Error: {e}"), false));
                        return Ok(None);
                    }
                }
            }
            "workflow.enabled" => {
                const MODES: &[&str] = &["auto", "on", "off"];
                let current = config.workflow.enabled.clone();
                let idx = MODES.iter().position(|&m| m == current).unwrap_or(0);
                let next_idx = if forward {
                    (idx + 1) % MODES.len()
                } else {
                    (idx + MODES.len() - 1) % MODES.len()
                };
                let new_val = MODES[next_idx];
                match config.apply_setting("workflow.enabled", new_val) {
                    Ok(confirmation) => {
                        if let Err(e) = self.save_setting("workflow.enabled", new_val) {
                            self.status_message = Some((format!("Save failed: {e}"), false));
                            return Ok(None);
                        }
                        self.status_message = Some((format!("Updated: {confirmation}"), true));
                        actions.push(AppAction::UpdateSetting {
                            key: "workflow.enabled".to_string(),
                            value: new_val.to_string(),
                        });
                    }
                    Err(e) => {
                        self.status_message = Some((format!("Error: {e}"), false));
                        return Ok(None);
                    }
                }
            }
            _ => return Ok(None),
        }

        // Return the first action (provider change is the primary one)
        Ok(actions.into_iter().next())
    }

    fn step_float(
        &mut self,
        config: &mut Config,
        increase: bool,
    ) -> anyhow::Result<Option<AppAction>> {
        let entry = &self.entries[self.selected];
        let current = self.current_value(config, entry.key);
        let val: f64 = current.parse().unwrap_or(0.0);

        let (step, min, max) = match entry.key {
            "budget.warning_threshold" => (0.01, 0.0, 1.0),
            _ => (0.1, 0.0, 2.0), // temperature
        };

        let new_val = if increase {
            (val + step).min(max)
        } else {
            (val - step).max(min)
        };

        // Round to avoid floating point drift
        let decimals = if step < 0.1 { 2 } else { 1 };
        let formatted = format!("{new_val:.decimals$}");

        match config.apply_setting(entry.key, &formatted) {
            Ok(confirmation) => {
                if let Err(e) = self.save_setting(entry.key, &formatted) {
                    self.status_message = Some((format!("Save failed: {e}"), false));
                    return Ok(None);
                }
                self.status_message = Some((format!("Updated: {confirmation}"), true));
                Ok(Some(AppAction::UpdateSetting {
                    key: entry.key.to_string(),
                    value: formatted,
                }))
            }
            Err(e) => {
                self.status_message = Some((format!("Error: {e}"), false));
                Ok(None)
            }
        }
    }

    fn toggle_bool(&mut self, config: &mut Config) -> anyhow::Result<Option<AppAction>> {
        let entry = &self.entries[self.selected];
        let current = self.current_value(config, entry.key);
        let new_val = if current == "true" { "false" } else { "true" };
        match config.apply_setting(entry.key, new_val) {
            Ok(confirmation) => {
                if let Err(e) = self.save_setting(entry.key, new_val) {
                    self.status_message = Some((format!("Save failed: {e}"), false));
                    return Ok(None);
                }
                self.status_message = Some((format!("Updated: {confirmation}"), true));
                Ok(Some(AppAction::UpdateSetting {
                    key: entry.key.to_string(),
                    value: new_val.to_string(),
                }))
            }
            Err(e) => {
                self.status_message = Some((format!("Error: {e}"), false));
                Ok(None)
            }
        }
    }

    /// Save a setting to DB. Returns an error if the DB connection is unavailable.
    fn save_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No database connection"))?;
        db.set_setting(key, value)?;
        Ok(())
    }

    /// Reset all settings to compiled defaults by clearing DB overrides and reloading config.
    fn reset_all_to_defaults(&mut self, config: &mut Config) -> anyhow::Result<Option<AppAction>> {
        let mut failed = 0usize;
        if let Some(ref db) = self.db {
            for entry in self.entries {
                if let Err(e) = db.delete_setting(entry.key) {
                    tracing::warn!("Failed to reset setting '{}': {e}", entry.key);
                    failed += 1;
                }
            }
        }
        // Reload config from disk (TOML + defaults, no DB overrides).
        // If the config file doesn't exist or can't be read, fall back to compiled defaults.
        *config = Config::load_from_db().unwrap_or_default();
        self.sync_select_indices(config);
        if failed > 0 {
            self.status_message = Some((
                format!("Reset done, but {failed} setting(s) failed to clear from DB"),
                false,
            ));
        } else {
            self.status_message = Some(("All settings reset to defaults".to_string(), true));
        }
        Ok(Some(AppAction::ConfigReloaded))
    }

    /// Get the source of a setting value.
    fn setting_source(&self, key: &str) -> SettingSource {
        if let Some(ref db) = self.db {
            if let Ok(Some(_)) = db.get_setting(key) {
                return SettingSource::Database;
            }
        }
        SettingSource::Default
    }

    pub fn render(&self, frame: &mut Frame, config: &Config) {
        if !self.visible {
            return;
        }

        let area = frame.area();
        let popup_area = popup_utils::popup_area(area);

        // Model selection list mode — render dedicated list
        if let EditMode::SelectingModel {
            selected,
            scroll_offset,
        } = &self.mode
        {
            let inner = popup_utils::render_popup_frame(frame, popup_area, "Select Model");
            if inner.height < 5 || inner.width < 12 {
                return;
            }

            let provider_id = PROVIDERS
                .get(self.provider_index)
                .map(|(id, _, _)| *id)
                .unwrap_or("openrouter");
            let models = models_for_provider(provider_id);
            let content_height = (inner.height as usize).saturating_sub(2); // reserve footer + status

            // Build model list lines
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, (_, display)) in models.iter().enumerate() {
                let is_current = i == self.model_index;
                let is_sel = i == *selected;
                let bullet = if is_current { theme::BULLET } else { "○" };
                let label = format!("  {bullet} {display}");

                let style = if is_sel {
                    theme::popup_selected()
                } else {
                    ratatui::style::Style::default()
                };

                let pad = (inner.width as usize).saturating_sub(label.len() + 1);
                lines.push(Line::from(vec![
                    Span::styled(label, style),
                    Span::styled(format!("{:>pad$} ", "", pad = pad), style),
                ]));
            }

            // Scroll to keep selected visible
            let adj_scroll = if *selected >= *scroll_offset + content_height {
                *selected - content_height + 1
            } else if *selected < *scroll_offset {
                *selected
            } else {
                *scroll_offset
            };

            let visible_lines: Vec<Line<'static>> = lines
                .into_iter()
                .skip(adj_scroll)
                .take(content_height)
                .collect();

            let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
            frame.render_widget(Paragraph::new(visible_lines), content_area);

            popup_utils::render_footer(
                frame,
                inner,
                " \u{2191}\u{2193}: navigate  Enter: select  Esc: cancel",
            );
            return;
        }

        let inner = popup_utils::render_popup_frame(frame, popup_area, "Settings");

        if inner.height < 5 || inner.width < 12 {
            return;
        }

        let content_height = (inner.height as usize).saturating_sub(3); // reserve footer + hint + status
        let mut lines: Vec<Line<'static>> = Vec::new();

        let mut last_category: Option<&str> = None;
        let mut row_indices: Vec<usize> = Vec::new(); // maps entry index to line index

        for (i, entry) in self.entries.iter().enumerate() {
            if last_category != Some(entry.category) {
                if last_category.is_some() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    format!(" {}", entry.category),
                    theme::bold(),
                )));
                last_category = Some(entry.category);
            }

            row_indices.push(lines.len());
            let value = self.current_value(config, entry.key);
            let source = self.setting_source(entry.key);
            let is_customized = !matches!(source, SettingSource::Default);
            let is_selected = i == self.selected;

            let display_value = if is_selected {
                if let EditMode::Editing { ref buffer } = self.mode {
                    format!("{buffer}_")
                } else if entry.kind == SettingKind::Bool {
                    let icon = if value == "true" {
                        theme::CHECK
                    } else {
                        theme::CROSS
                    };
                    icon.to_string()
                } else if entry.key == "model" {
                    format!("{value} \u{25B6}")
                } else if entry.kind == SettingKind::Select {
                    format!("< {value} >")
                } else {
                    value.clone()
                }
            } else if entry.kind == SettingKind::Bool {
                let icon = if value == "true" {
                    theme::CHECK
                } else {
                    theme::CROSS
                };
                icon.to_string()
            } else {
                value.clone()
            };

            let customized_suffix = if is_customized { " \u{25CF}" } else { "" };
            let label_width = inner.width.saturating_sub(4) as usize;
            let label = format!("  {}", entry.label);
            let val_with_suffix = format!("{display_value}{customized_suffix}");
            let val_len = val_with_suffix.len();
            let padding = label_width.saturating_sub(label.len() + val_len);

            let base_style = if is_selected {
                theme::popup_selected()
            } else {
                ratatui::style::Style::default()
            };

            let mut spans = vec![
                Span::styled(label.clone(), base_style),
                Span::styled(format!("{:>pad$}", "", pad = padding), base_style),
                Span::styled(display_value, base_style),
            ];
            if is_customized {
                spans.push(Span::styled(" \u{25CF}", base_style.patch(theme::dim())));
            }
            // Fill remaining space with base style for consistent highlight
            spans.push(Span::styled(" ", base_style));

            lines.push(Line::from(spans));
        }

        // Scroll to keep selected visible
        let selected_line = row_indices.get(self.selected).copied().unwrap_or(0);
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

        // Status line
        popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 3);

        // CLI hint
        let cli_hint_y = inner.y + inner.height - 2;
        let cli_hint_area = Rect::new(inner.x, cli_hint_y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " More: borg settings set <key> <value>".to_string(),
                theme::dim(),
            ))),
            cli_hint_area,
        );

        // Footer hint (context-sensitive)
        let hint = match self.mode {
            EditMode::Browsing => {
                let entry = &self.entries[self.selected];
                if entry.key == "model" {
                    " Enter: choose model  r: reset  Esc: close"
                } else {
                    match entry.kind {
                        SettingKind::Bool => " Space: toggle  Enter: apply  r: reset  Esc: close",
                        SettingKind::Select => {
                            " \u{25C0}\u{25B6}: cycle  Enter: apply  r: reset  Esc: close"
                        }
                        SettingKind::Float => {
                            " \u{25C0}\u{25B6}: adjust  Enter: edit  r: reset  Esc: close"
                        }
                        _ => " Enter: edit  r: reset  Esc: close",
                    }
                }
            }
            EditMode::ConfirmReset => {
                " Reset all settings to defaults? y: confirm  any key: cancel"
            }
            EditMode::Editing { .. } => " Enter: apply  Esc: cancel",
            EditMode::SelectingModel { .. } => {
                " \u{2191}\u{2193}: navigate  Enter: select  Esc: cancel"
            }
        };
        popup_utils::render_footer(frame, inner, hint);
    }
}

impl PopupHandler for SettingsPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> anyhow::Result<Option<AppAction>> {
        self.handle_key(key, config)
    }

    fn handle_paste_event(&mut self, text: &str) -> bool {
        self.handle_paste(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_and_dismiss() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        assert!(!popup.is_visible());
        popup.show(&cfg);
        assert!(popup.is_visible());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn navigation_wraps() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);
        let count = popup.entries.len();

        assert_eq!(popup.selected, 0);

        // Up from 0 wraps to last
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut cfg = Config::default();

        popup.handle_key(up, &mut cfg).unwrap();
        assert_eq!(popup.selected, count - 1);

        popup.handle_key(down, &mut cfg).unwrap();
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn bool_toggle() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);

        // Find sandbox.enabled (index 7 — under Security)
        popup.selected = 7;
        assert_eq!(popup.entries[7].key, "sandbox.enabled");

        let mut cfg = Config::default();
        assert!(cfg.sandbox.enabled);

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let result = popup.handle_key(space, &mut cfg).unwrap();
        assert!(!cfg.sandbox.enabled);
        assert!(result.is_some());
    }

    #[test]
    fn valid_edit_applies() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);

        // Select temperature (index 2)
        popup.selected = 2;
        assert_eq!(popup.entries[2].key, "temperature");

        let mut cfg = Config::default();

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // Start editing
        popup.handle_key(enter, &mut cfg).unwrap();
        assert!(matches!(popup.mode, EditMode::Editing { .. }));

        // Clear buffer and type "1.5"
        if let EditMode::Editing { ref mut buffer } = popup.mode {
            buffer.clear();
            buffer.push_str("1.5");
        }

        // Apply
        let result = popup.handle_key(enter, &mut cfg).unwrap();
        assert!((cfg.llm.temperature - 1.5).abs() < f32::EPSILON);
        assert!(result.is_some());
        assert!(matches!(popup.mode, EditMode::Browsing));
    }

    #[test]
    fn invalid_edit_shows_error() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);

        // Select temperature (index 2)
        popup.selected = 2;
        let mut cfg = Config::default();
        let original_temp = cfg.llm.temperature;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // Start editing
        popup.handle_key(enter, &mut cfg).unwrap();

        // Set invalid value
        if let EditMode::Editing { ref mut buffer } = popup.mode {
            buffer.clear();
            buffer.push_str("5.0"); // out of range
        }

        // Apply — should fail
        let result = popup.handle_key(enter, &mut cfg).unwrap();
        assert!((cfg.llm.temperature - original_temp).abs() < f32::EPSILON);
        assert!(result.is_none());
        assert!(popup.status_message.as_ref().map_or(false, |(_, ok)| !ok));
    }

    #[test]
    fn all_settings_covered() {
        let popup = SettingsPopup::new();
        assert_eq!(popup.entries.len(), 16);

        let cfg = Config::default();
        for entry in popup.entries {
            let val = popup.current_value(&cfg, entry.key);
            assert_ne!(val, "?", "missing current_value for {}", entry.key);
        }
    }

    #[test]
    fn esc_during_edit_cancels() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Select temperature (index 2) — model is now Select, not editable via Enter
        popup.selected = 2;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        popup.handle_key(enter, &mut cfg).unwrap();
        assert!(matches!(popup.mode, EditMode::Editing { .. }));

        popup.handle_key(esc, &mut cfg).unwrap();
        assert!(matches!(popup.mode, EditMode::Browsing));
        assert!(popup.is_visible());
    }

    #[test]
    fn tab_does_nothing() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);

        // Select sandbox.enabled (Bool at index 7)
        popup.selected = 7;
        assert_eq!(popup.entries[7].key, "sandbox.enabled");
        let mut cfg = Config::default();
        let original = cfg.sandbox.enabled;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let result = popup.handle_key(tab, &mut cfg).unwrap();

        assert_eq!(cfg.sandbox.enabled, original);
        assert!(result.is_none());
        assert!(matches!(popup.mode, EditMode::Browsing));
    }

    #[test]
    fn provider_cycle_forward() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Provider is entry 0
        popup.selected = 0;
        assert_eq!(popup.entries[0].key, "provider");
        let initial_provider_index = popup.provider_index;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let result = popup.handle_key(right, &mut cfg).unwrap();
        assert!(result.is_some());
        assert_eq!(
            popup.provider_index,
            (initial_provider_index + 1) % PROVIDERS.len()
        );
        // Model index should reset to 0
        assert_eq!(popup.model_index, 0);
    }

    #[test]
    fn provider_cycle_backward() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 0;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let result = popup.handle_key(left, &mut cfg).unwrap();
        assert!(result.is_some());
        // From 0, should wrap to last
        assert_eq!(popup.provider_index, PROVIDERS.len() - 1);
    }

    #[test]
    fn model_enter_opens_selection() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Model is entry 1
        popup.selected = 1;
        assert_eq!(popup.entries[1].key, "model");

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = popup.handle_key(enter, &mut cfg).unwrap();
        assert!(result.is_none());
        assert!(matches!(popup.mode, EditMode::SelectingModel { .. }));
    }

    #[test]
    fn float_step_right_increases() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Temperature is entry 2
        popup.selected = 2;
        let original = cfg.llm.temperature;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let result = popup.handle_key(right, &mut cfg).unwrap();
        assert!(result.is_some());
        assert!((cfg.llm.temperature - (original + 0.1)).abs() < 0.01);
    }

    #[test]
    fn float_step_left_decreases() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 2;
        let original = cfg.llm.temperature;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let result = popup.handle_key(left, &mut cfg).unwrap();
        assert!(result.is_some());
        assert!((cfg.llm.temperature - (original - 0.1)).abs() < 0.01);
    }

    #[test]
    fn select_enter_dismisses() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Provider is entry 0 — Enter should dismiss (apply), not cycle
        popup.selected = 0;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        popup.handle_key(enter, &mut cfg).unwrap();
        // Should dismiss the popup
        assert!(!popup.is_visible());
        // Should NOT have cycled
        assert_eq!(popup.provider_index, 0);
    }

    #[test]
    fn sync_indices_from_config() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        cfg.llm.provider = Some("anthropic".to_string());
        cfg.llm.model = "claude-haiku-4".to_string();
        popup.show(&cfg);

        assert_eq!(popup.provider_index, 2); // anthropic is index 2
        assert_eq!(popup.model_index, 2); // claude-haiku-4 is index 2 in ANTHROPIC_MODELS
    }

    #[test]
    fn float_clamps_at_max() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        cfg.llm.temperature = 2.0;
        popup.show(&cfg);

        popup.selected = 2; // temperature

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        popup.handle_key(right, &mut cfg).unwrap();
        // Should stay at 2.0
        assert!((cfg.llm.temperature - 2.0).abs() < 0.01);
    }

    #[test]
    fn float_clamps_at_min() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        cfg.llm.temperature = 0.0;
        popup.show(&cfg);

        popup.selected = 2; // temperature

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        popup.handle_key(left, &mut cfg).unwrap();
        assert!((cfg.llm.temperature - 0.0).abs() < 0.01);
    }

    #[test]
    fn provider_change_updates_model_config() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Start at openrouter (index 0), cycle to openai (index 1)
        popup.selected = 0;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        popup.handle_key(right, &mut cfg).unwrap();

        // Config should now have openai provider and first openai model
        assert_eq!(cfg.llm.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.llm.model, "gpt-4.1");
    }

    #[test]
    fn model_right_opens_selection() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 1; // model
        assert_eq!(popup.entries[1].key, "model");

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        popup.handle_key(right, &mut cfg).unwrap();
        assert!(matches!(popup.mode, EditMode::SelectingModel { .. }));
    }

    #[test]
    fn model_left_opens_selection() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 1; // model

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        popup.handle_key(left, &mut cfg).unwrap();
        assert!(matches!(popup.mode, EditMode::SelectingModel { .. }));
    }

    #[test]
    fn model_selection_up_down_navigates() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Enter model selection
        popup.mode = EditMode::SelectingModel {
            selected: 0,
            scroll_offset: 0,
        };

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        popup.handle_key(down, &mut cfg).unwrap();
        if let EditMode::SelectingModel { selected, .. } = popup.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected SelectingModel");
        }

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        popup.handle_key(up, &mut cfg).unwrap();
        if let EditMode::SelectingModel { selected, .. } = popup.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected SelectingModel");
        }
    }

    #[test]
    fn model_selection_wraps() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Start at first model, go up to wrap
        popup.mode = EditMode::SelectingModel {
            selected: 0,
            scroll_offset: 0,
        };

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        popup.handle_key(up, &mut cfg).unwrap();

        let provider_id = PROVIDERS[popup.provider_index].0;
        let count = models_for_provider(provider_id).len();
        if let EditMode::SelectingModel { selected, .. } = popup.mode {
            assert_eq!(selected, count - 1);
        } else {
            panic!("expected SelectingModel");
        }
    }

    #[test]
    fn model_selection_enter_applies() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Select second model in current provider's list
        popup.mode = EditMode::SelectingModel {
            selected: 1,
            scroll_offset: 0,
        };

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = popup.handle_key(enter, &mut cfg).unwrap();

        assert!(result.is_some());
        assert!(matches!(popup.mode, EditMode::Browsing));
        assert_eq!(popup.model_index, 1);

        let provider_id = PROVIDERS[popup.provider_index].0;
        let models = models_for_provider(provider_id);
        assert_eq!(cfg.llm.model, models[1].0);
    }

    #[test]
    fn model_selection_esc_cancels() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        let original_model = cfg.llm.model.clone();
        popup.show(&cfg);

        popup.mode = EditMode::SelectingModel {
            selected: 2,
            scroll_offset: 0,
        };

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        popup.handle_key(esc, &mut cfg).unwrap();

        assert!(matches!(popup.mode, EditMode::Browsing));
        assert_eq!(cfg.llm.model, original_model); // unchanged
    }

    #[test]
    fn left_right_noop_on_uint_fields() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // conversation.max_iterations is a Uint field (index 4)
        popup.selected = 4;
        assert_eq!(popup.entries[4].key, "conversation.max_iterations");

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let result = popup.handle_key(right, &mut cfg).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bool_display_uses_check_and_cross() {
        let popup = SettingsPopup::new();
        let cfg = Config::default();

        // sandbox.enabled defaults to true
        let val = popup.current_value(&cfg, "sandbox.enabled");
        assert_eq!(val, "true");

        // The display should use ✓ for true bools — verify via current_value + formatting logic
        // (render builds the display_value inline; we test the component values here)
        assert_eq!(theme::CHECK, "✓");
        assert_eq!(theme::CROSS, "✗");
    }

    #[test]
    fn display_value_no_source_tags() {
        let popup = SettingsPopup::new();
        let cfg = Config::default();

        // Verify no current_value contains [db] or [toml] — those are gone
        for entry in popup.entries {
            let val = popup.current_value(&cfg, entry.key);
            assert!(
                !val.contains("[db]") && !val.contains("[toml]"),
                "source tag found in value for {}: {}",
                entry.key,
                val
            );
        }
    }

    #[test]
    fn default_values_not_customized() {
        // Use a popup with no DB so real user settings don't interfere
        let popup = SettingsPopup {
            visible: false,
            entries: SETTINGS,
            selected: 0,
            mode: EditMode::Browsing,
            status_message: None,
            db: None,
            provider_index: 0,
            model_index: 0,
        };

        // With no DB, all sources should be Default
        for entry in popup.entries {
            let source = popup.setting_source(entry.key);
            assert_eq!(
                source,
                SettingSource::Default,
                "expected Default source for {} but got {:?}",
                entry.key,
                source
            );
        }
    }

    #[test]
    fn r_key_enters_confirm_reset() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        popup.handle_key(r, &mut cfg).unwrap();
        assert_eq!(popup.mode, EditMode::ConfirmReset);
    }

    #[test]
    fn confirm_reset_cancel_on_non_y() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // Enter confirm mode
        popup.mode = EditMode::ConfirmReset;

        // Press 'n' — should cancel
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        popup.handle_key(n, &mut cfg).unwrap();
        assert_eq!(popup.mode, EditMode::Browsing);
    }

    #[test]
    fn confirm_reset_y_resets_config() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        // Modify config to non-default
        cfg.llm.temperature = 1.5;
        cfg.sandbox.enabled = false;

        // Enter confirm mode and confirm
        popup.mode = EditMode::ConfirmReset;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let result = popup.handle_key(y, &mut cfg).unwrap();

        // Config should be reloaded (no longer the modified values)
        assert!(
            (cfg.llm.temperature - 1.5).abs() > f32::EPSILON || cfg.sandbox.enabled,
            "config was not reloaded after reset"
        );
        assert_eq!(popup.mode, EditMode::Browsing);
        assert!(result.is_some()); // ConfigReloaded action
        assert!(
            popup
                .status_message
                .as_ref()
                .map_or(false, |(msg, ok)| *ok && msg.contains("reset")),
            "expected success status message about reset"
        );
    }

    #[test]
    fn confirm_reset_esc_cancels() {
        let mut popup = SettingsPopup::new();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.mode = EditMode::ConfirmReset;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        popup.handle_key(esc, &mut cfg).unwrap();
        assert_eq!(popup.mode, EditMode::Browsing);
    }

    #[test]
    fn handle_paste_consumed_in_editing_mode() {
        let mut popup = SettingsPopup::new();
        let cfg = Config::default();
        popup.show(&cfg);

        // Paste during Browsing should NOT be consumed
        assert!(!popup.handle_paste("anything"));

        // Enter editing mode
        popup.mode = EditMode::Editing {
            buffer: String::new(),
        };
        assert!(popup.handle_paste("pasted-value"));
        if let EditMode::Editing { ref buffer } = popup.mode {
            assert_eq!(buffer, "pasted-value");
        } else {
            panic!("expected Editing mode");
        }
    }

    #[test]
    fn handle_paste_not_consumed_when_hidden() {
        let popup = &mut SettingsPopup::new();
        assert!(!popup.handle_paste("anything"));
    }

    /// Helper: create a SettingsPopup backed by a read-only SQLite DB so all
    /// writes fail.  Uses a temp file that is opened normally (so migrations
    /// run), then re-opened as read-only.
    fn popup_with_readonly_db() -> SettingsPopup {
        use borg_core::db::Database;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("readonly.db");

        // Create & migrate normally
        {
            let conn = rusqlite::Connection::open(&path).expect("create db");
            let _db = Database::from_connection(conn).expect("init db");
        }
        // Re-open read-only
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
        let conn = rusqlite::Connection::open_with_flags(&path, flags).expect("open ro");
        let db = Database::from_connection(conn).expect("wrap ro db");

        let mut popup = SettingsPopup::new();
        popup.db = Some(db);
        // Keep the tempdir alive by leaking it (test-only, small).
        std::mem::forget(dir);
        popup
    }

    #[test]
    fn save_failure_shows_error_when_db_is_none() {
        // Simulate the case where Database::open() failed at construction
        let mut popup = SettingsPopup::new();
        popup.db = None;
        let cfg = Config::default();
        popup.show(&cfg);

        // Select sandbox.enabled (Bool at index 7)
        popup.selected = 7;
        assert_eq!(popup.entries[7].key, "sandbox.enabled");

        let mut cfg = Config::default();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);

        let result = popup.handle_key(space, &mut cfg).unwrap();
        // Should NOT return an action (no DB to save to)
        assert!(result.is_none());
        let (msg, is_success) = popup.status_message.as_ref().expect("status message set");
        assert!(!is_success, "should be an error, not success");
        assert!(
            msg.contains("Save failed") || msg.contains("No database"),
            "unexpected status message: {msg}"
        );
    }

    #[test]
    fn save_failure_shows_error_on_bool_toggle() {
        let mut popup = popup_with_readonly_db();
        let cfg = Config::default();
        popup.show(&cfg);

        // Select sandbox.enabled (Bool at index 7)
        popup.selected = 7;
        assert_eq!(popup.entries[7].key, "sandbox.enabled");

        let mut cfg = Config::default();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);

        let result = popup.handle_key(space, &mut cfg).unwrap();
        // Should NOT return an action (save failed)
        assert!(result.is_none());
        // Should show error message
        let (msg, is_success) = popup.status_message.as_ref().expect("status message set");
        assert!(!is_success, "should be an error, not success");
        assert!(
            msg.contains("Save failed") || msg.contains("save failed") || msg.contains("readonly"),
            "unexpected status message: {msg}"
        );
    }

    #[test]
    fn save_failure_shows_error_on_provider_cycle() {
        let mut popup = popup_with_readonly_db();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 0; // provider
        assert_eq!(popup.entries[0].key, "provider");

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        let result = popup.handle_key(right, &mut cfg).unwrap();
        assert!(result.is_none());
        let (msg, is_success) = popup.status_message.as_ref().expect("status message set");
        assert!(!is_success);
        assert!(
            msg.contains("Save failed") || msg.contains("readonly"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn save_failure_shows_error_on_float_step() {
        let mut popup = popup_with_readonly_db();
        let mut cfg = Config::default();
        popup.show(&cfg);

        popup.selected = 2; // temperature
        assert_eq!(popup.entries[2].key, "temperature");

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        let result = popup.handle_key(right, &mut cfg).unwrap();
        assert!(result.is_none());
        let (msg, is_success) = popup.status_message.as_ref().expect("status message set");
        assert!(!is_success);
        assert!(
            msg.contains("Save failed") || msg.contains("readonly"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn reset_all_reports_failures_on_readonly_db() {
        let mut popup = popup_with_readonly_db();
        let mut cfg = Config::default();
        popup.show(&cfg);

        let result = popup.reset_all_to_defaults(&mut cfg).unwrap();
        assert!(result.is_some()); // ConfigReloaded still returned
        let (msg, is_success) = popup.status_message.as_ref().expect("status message set");
        assert!(!is_success, "should report failures");
        assert!(msg.contains("failed to clear"), "unexpected: {msg}");
    }
}
