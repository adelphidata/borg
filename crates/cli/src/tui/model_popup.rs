//! `/model` popup: pick provider + model, optionally enter an API key when the
//! chosen provider has no resolvable credentials yet.
//!
//! Overlaps `/settings` deliberately — this is the fast path for "swap my
//! model" without wading through the full settings popup.

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use borg_core::config::Config;
use borg_core::db::Database;

use crate::api_key_store::{self, ApiKeySaveOutcome};
use crate::onboarding::{models_for_provider, PROVIDERS};

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

const MASK_CHAR: char = '•';

/// Steps in the model-switching flow.
#[derive(Clone, Debug)]
enum Step {
    PickProvider {
        cursor: usize,
    },
    PickModel {
        cursor: usize,
    },
    /// API-key entry. `buffer` holds the literal input; the renderer masks it.
    EnterApiKey {
        buffer: String,
    },
}

pub struct ModelPopup {
    visible: bool,
    step: Step,
    /// Index into `PROVIDERS` once the user has picked a provider.
    chosen_provider_idx: Option<usize>,
    status_message: Option<(String, bool)>,
    db: Option<Database>,
}

impl ModelPopup {
    pub fn new() -> Self {
        let db = match Database::open() {
            Ok(db) => Some(db),
            Err(e) => {
                tracing::warn!("Model popup: failed to open database: {e}");
                None
            }
        };
        Self {
            visible: false,
            step: Step::PickProvider { cursor: 0 },
            chosen_provider_idx: None,
            status_message: None,
            db,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Open the popup, seeding the provider cursor to the current provider.
    pub fn show(&mut self, config: &Config) {
        self.visible = true;
        self.chosen_provider_idx = None;
        self.status_message = None;

        let current_provider = config.llm.provider.as_deref().unwrap_or("openrouter");
        let cursor = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == current_provider)
            .unwrap_or(0);
        self.step = Step::PickProvider { cursor };
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.status_message = None;
    }

    /// Provider ID at the current picker cursor, or of the chosen provider.
    fn provider_id(&self) -> Option<&'static str> {
        let idx = match &self.step {
            Step::PickProvider { cursor } => *cursor,
            _ => self.chosen_provider_idx?,
        };
        PROVIDERS.get(idx).map(|(id, _, _)| *id)
    }

    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;

        if !self.visible {
            return Ok(None);
        }

        if matches!(key.code, KeyCode::Esc) {
            self.dismiss();
            return Ok(None);
        }

        match self.step.clone() {
            Step::PickProvider { cursor } => self.handle_pick_provider(key, cursor, config),
            Step::PickModel { cursor } => self.handle_pick_model(key, cursor, config),
            Step::EnterApiKey { buffer } => self.handle_enter_api_key(key, buffer, config),
        }
    }

    fn handle_pick_provider(
        &mut self,
        key: crossterm::event::KeyEvent,
        cursor: usize,
        _config: &Config,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;
        let total = PROVIDERS.len();

        match key.code {
            KeyCode::Up => {
                let next = if cursor == 0 { total - 1 } else { cursor - 1 };
                self.step = Step::PickProvider { cursor: next };
            }
            KeyCode::Down => {
                self.step = Step::PickProvider {
                    cursor: (cursor + 1) % total,
                };
            }
            KeyCode::Enter => {
                self.chosen_provider_idx = Some(cursor);
                self.step = Step::PickModel { cursor: 0 };
            }
            _ => {}
        }
        Ok(None)
    }

    fn handle_pick_model(
        &mut self,
        key: crossterm::event::KeyEvent,
        cursor: usize,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;
        let Some(provider_id) = self.provider_id() else {
            self.dismiss();
            return Ok(None);
        };
        let models = models_for_provider(provider_id);
        if models.is_empty() {
            // No curated model list — jump straight to save with current model.
            return self.persist_selection(provider_id, None, config);
        }
        let total = models.len();

        match key.code {
            KeyCode::Up => {
                let next = if cursor == 0 { total - 1 } else { cursor - 1 };
                self.step = Step::PickModel { cursor: next };
            }
            KeyCode::Down => {
                self.step = Step::PickModel {
                    cursor: (cursor + 1) % total,
                };
            }
            KeyCode::Backspace => {
                let prev = self
                    .chosen_provider_idx
                    .take()
                    .and_then(|idx| Some(idx).filter(|&i| i < PROVIDERS.len()))
                    .unwrap_or(0);
                self.step = Step::PickProvider { cursor: prev };
            }
            KeyCode::Enter => {
                let model_id = models[cursor].0.clone();
                return self.persist_selection(provider_id, Some(&model_id), config);
            }
            _ => {}
        }
        Ok(None)
    }

    fn handle_enter_api_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut buffer: String,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        use crossterm::event::KeyCode;
        let Some(provider_id) = self.provider_id() else {
            self.dismiss();
            return Ok(None);
        };

        match key.code {
            KeyCode::Backspace => {
                buffer.pop();
                self.step = Step::EnterApiKey { buffer };
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                self.step = Step::EnterApiKey { buffer };
            }
            KeyCode::Enter => {
                return self.save_api_key_and_close(provider_id, &buffer, config);
            }
            _ => {}
        }
        Ok(None)
    }

    /// Bracketed-paste support while in `EnterApiKey`.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.visible {
            return false;
        }
        if let Step::EnterApiKey { mut buffer } = self.step.clone() {
            buffer.push_str(text);
            self.step = Step::EnterApiKey { buffer };
            return true;
        }
        false
    }

    /// Persist the chosen provider/model to the live config + DB. If the
    /// provider needs a key and none resolves, advance to `EnterApiKey`
    /// instead of closing.
    fn persist_selection(
        &mut self,
        provider_id: &'static str,
        model_id: Option<&str>,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        if let Err(e) = config.apply_setting("provider", provider_id) {
            self.status_message = Some((format!("Error: {e}"), false));
            return Ok(None);
        }
        if let Some(m) = model_id {
            if let Err(e) = config.apply_setting("model", m) {
                self.status_message = Some((format!("Error: {e}"), false));
                return Ok(None);
            }
        }

        if let Some(ref db) = self.db {
            if let Err(e) = db.set_setting("provider", provider_id) {
                self.status_message = Some((format!("DB save failed: {e}"), false));
                return Ok(None);
            }
            if let Some(m) = model_id {
                if let Err(e) = db.set_setting("model", m) {
                    self.status_message = Some((format!("DB save failed: {e}"), false));
                    return Ok(None);
                }
            }
        }

        // Does the new provider have a resolvable key? If not, prompt for one.
        if provider_needs_key(provider_id) && !has_resolvable_key(config) {
            self.step = Step::EnterApiKey {
                buffer: String::new(),
            };
            self.status_message = Some((
                format!("No API key found for {provider_id}. Enter one:"),
                true,
            ));
            return Ok(None);
        }

        let confirmation = match model_id {
            Some(m) => format!("provider = {provider_id}, model = {m}"),
            None => format!("provider = {provider_id}"),
        };
        self.status_message = Some((format!("Updated: {confirmation}"), true));
        self.dismiss();
        Ok(Some(AppAction::ConfigReloaded))
    }

    fn save_api_key_and_close(
        &mut self,
        provider_id: &str,
        raw_key: &str,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        let Some(ref db) = self.db else {
            self.status_message = Some(("Database unavailable".to_string(), false));
            return Ok(None);
        };

        match api_key_store::save_api_key(db, config, provider_id, raw_key)? {
            ApiKeySaveOutcome::StoredInKeychain => {
                self.status_message = Some(("API key saved to keychain".to_string(), true));
                self.dismiss();
                Ok(Some(AppAction::ConfigReloaded))
            }
            ApiKeySaveOutcome::KeychainUnavailable => {
                self.status_message = Some((api_key_store::env_var_hint(provider_id), false));
                Ok(None)
            }
            ApiKeySaveOutcome::EmptyInput => {
                self.status_message = Some(("Enter a non-empty key".to_string(), false));
                Ok(None)
            }
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Switch Model", 5, 2)
        else {
            return;
        };

        match &self.step {
            Step::PickProvider { cursor } => {
                self.render_list(
                    frame,
                    inner,
                    content_height,
                    "Provider",
                    PROVIDERS
                        .iter()
                        .map(|(_id, display, desc)| (*display, *desc))
                        .collect::<Vec<_>>(),
                    *cursor,
                );
                popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);
                popup_utils::render_footer(frame, inner, " ↑↓: navigate  Enter: pick  Esc: close");
            }
            Step::PickModel { cursor } => {
                let provider_id = self.provider_id().unwrap_or("openrouter");
                let live = models_for_provider(provider_id);
                let models: Vec<(&str, &str)> = live
                    .iter()
                    .map(|(id, label)| (id.as_str(), label.as_str()))
                    .collect();
                self.render_list(
                    frame,
                    inner,
                    content_height,
                    &format!("Model ({provider_id})"),
                    models,
                    *cursor,
                );
                popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);
                popup_utils::render_footer(
                    frame,
                    inner,
                    " ↑↓: navigate  Enter: pick  Backspace: back  Esc: close",
                );
            }
            Step::EnterApiKey { buffer } => {
                let provider_id = self.provider_id().unwrap_or("?");
                let masked: String = buffer.chars().map(|_| MASK_CHAR).collect();
                let lines = vec![
                    Line::from(Span::styled(
                        format!(" API key for {provider_id} (masked):"),
                        theme::dim(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(format!(" {masked}_"), theme::popup_selected())),
                ];
                let area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
                frame.render_widget(Paragraph::new(lines), area);
                popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);
                popup_utils::render_footer(frame, inner, " Enter: save  Esc: cancel");
            }
        }
    }

    fn render_list(
        &self,
        frame: &mut Frame,
        inner: Rect,
        content_height: usize,
        header: &str,
        items: Vec<(&str, &str)>,
        cursor: usize,
    ) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(format!(" {header}"), theme::dim())));
        lines.push(Line::from(""));

        for (i, (id, desc)) in items.iter().enumerate() {
            let marker = if i == cursor { "▶ " } else { "  " };
            let label = format!("{marker}{id:<30}  {desc}");
            let style = if i == cursor {
                theme::popup_selected()
            } else {
                theme::dim()
            };
            lines.push(Line::from(Span::styled(label, style)));
        }

        // Simple scroll: keep cursor visible.
        let scroll_offset = if cursor + 2 >= content_height {
            cursor + 3 - content_height
        } else {
            0
        };
        let visible: Vec<Line<'static>> = lines.into_iter().skip(scroll_offset).collect();

        let area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible), area);
    }
}

fn provider_needs_key(provider_id: &str) -> bool {
    use std::str::FromStr;
    borg_core::provider::Provider::from_str(provider_id)
        .map(|p| p.requires_api_key())
        .unwrap_or(true)
}

fn has_resolvable_key(config: &Config) -> bool {
    // `resolve_api_keys` is the canonical entry point the agent loop uses,
    // so if it returns Ok we know the provider is usable.
    config.resolve_api_keys().is_ok()
}

impl PopupHandler for ModelPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> Result<Option<AppAction>> {
        self.handle_key(key, config)
    }

    fn handle_paste_event(&mut self, text: &str) -> bool {
        self.handle_paste(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_popup() -> ModelPopup {
        // Construct without opening a DB so tests can run hermetically.
        ModelPopup {
            visible: false,
            step: Step::PickProvider { cursor: 0 },
            chosen_provider_idx: None,
            status_message: None,
            db: None,
        }
    }

    #[test]
    fn show_and_dismiss() {
        let mut popup = make_popup();
        let cfg = Config::default();
        assert!(!popup.is_visible());
        popup.show(&cfg);
        assert!(popup.is_visible());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn esc_dismisses_without_changes() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        let original_provider = cfg.llm.provider.clone();
        popup.show(&cfg);
        popup.handle_key(key(KeyCode::Esc), &mut cfg).unwrap();
        assert!(!popup.is_visible());
        assert_eq!(cfg.llm.provider, original_provider);
    }

    #[test]
    fn show_seeds_cursor_to_current_provider() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        cfg.llm.provider = Some("anthropic".to_string());
        popup.show(&cfg);
        let expected_idx = PROVIDERS
            .iter()
            .position(|(id, _, _)| *id == "anthropic")
            .unwrap();
        match popup.step {
            Step::PickProvider { cursor } => assert_eq!(cursor, expected_idx),
            _ => panic!("expected PickProvider step"),
        }
    }

    #[test]
    fn pick_provider_advances_to_pick_model() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        popup.show(&cfg);
        popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap();
        assert!(matches!(popup.step, Step::PickModel { cursor: 0 }));
        assert!(popup.chosen_provider_idx.is_some());
    }

    #[test]
    fn provider_navigation_wraps() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        popup.show(&cfg);
        // Force cursor to 0
        popup.step = Step::PickProvider { cursor: 0 };
        popup.handle_key(key(KeyCode::Up), &mut cfg).unwrap();
        match popup.step {
            Step::PickProvider { cursor } => assert_eq!(cursor, PROVIDERS.len() - 1),
            _ => panic!("expected PickProvider"),
        }
        popup.handle_key(key(KeyCode::Down), &mut cfg).unwrap();
        match popup.step {
            Step::PickProvider { cursor } => assert_eq!(cursor, 0),
            _ => panic!("expected PickProvider"),
        }
    }

    #[test]
    fn model_pick_without_db_updates_config() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        // Pre-set a resolvable key so we skip the EnterApiKey step deterministically.
        cfg.llm.api_keys = vec![borg_core::secrets_resolve::SecretRef::Env {
            var: "BORG_TEST_FAKE_KEY".to_string(),
        }];
        // Safety: only the current test process reads this env var.
        unsafe { std::env::set_var("BORG_TEST_FAKE_KEY", "test-token") };

        popup.show(&cfg);
        popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap(); // pick provider
        let provider_id = popup.provider_id().unwrap();
        let expected_model = models_for_provider(provider_id)[0].0.clone();
        let action = popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap();

        assert_eq!(cfg.llm.model, expected_model);
        assert_eq!(cfg.llm.provider.as_deref(), Some(provider_id));
        assert!(!popup.is_visible(), "popup should close after save");
        assert!(matches!(action, Some(AppAction::ConfigReloaded)));

        unsafe { std::env::remove_var("BORG_TEST_FAKE_KEY") };
    }

    #[test]
    fn model_pick_advances_to_enter_api_key_when_unresolvable() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        // Make sure no api_key resolves. Config::default() has only
        // `api_key_env = "OPENROUTER_API_KEY"` — unset it for the test.
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::remove_var("GROQ_API_KEY");
        }

        popup.show(&cfg);
        // Advance to a provider that requires a key — OpenRouter at index 0.
        popup.step = Step::PickProvider { cursor: 0 };
        popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap();
        // Without a DB, persist_selection still mutates cfg and advances the step.
        popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap();
        assert!(
            matches!(popup.step, Step::EnterApiKey { .. }),
            "expected EnterApiKey, got {:?}",
            popup.step
        );
        assert!(popup.is_visible());
    }

    #[test]
    fn enter_api_key_buffer_holds_literal_chars() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        popup.visible = true;
        popup.chosen_provider_idx = Some(0);
        popup.step = Step::EnterApiKey {
            buffer: String::new(),
        };
        popup.handle_key(key(KeyCode::Char('a')), &mut cfg).unwrap();
        popup.handle_key(key(KeyCode::Char('b')), &mut cfg).unwrap();
        popup.handle_key(key(KeyCode::Char('c')), &mut cfg).unwrap();
        match &popup.step {
            Step::EnterApiKey { buffer } => assert_eq!(buffer, "abc"),
            _ => panic!("expected EnterApiKey"),
        }
        popup.handle_key(key(KeyCode::Backspace), &mut cfg).unwrap();
        match &popup.step {
            Step::EnterApiKey { buffer } => assert_eq!(buffer, "ab"),
            _ => panic!("expected EnterApiKey"),
        }
    }

    #[test]
    fn hidden_popup_ignores_keys() {
        let mut popup = make_popup();
        let mut cfg = Config::default();
        assert!(!popup.is_visible());
        let result = popup.handle_key(key(KeyCode::Enter), &mut cfg).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn provider_needs_key_helper() {
        assert!(provider_needs_key("openrouter"));
        assert!(provider_needs_key("anthropic"));
        assert!(!provider_needs_key("ollama"));
        assert!(!provider_needs_key("claude-cli"));
    }
}
