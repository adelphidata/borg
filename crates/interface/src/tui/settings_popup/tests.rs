use super::*;

/// Open the popup without a daemon client so tests stay hermetic.
/// Mirrors `SettingsPopup::show` minus the `refresh_customized_keys` RPC.
fn open(popup: &mut SettingsPopup, cfg: &Config) {
    popup.visible = true;
    popup.entries = visible_entries(cfg);
    popup.selected = 0;
    popup.mode = EditMode::Browsing;
    popup.status_message = None;
    popup.sync_select_indices(cfg);
}

#[test]
fn show_and_dismiss() {
    let mut popup = SettingsPopup::new();
    let cfg = Config::default();
    assert!(!popup.is_visible());
    open(&mut popup, &cfg);
    assert!(popup.is_visible());
    popup.dismiss();
    assert!(!popup.is_visible());
}

#[test]
fn navigation_wraps() {
    let mut popup = SettingsPopup::new();
    let cfg = Config::default();
    open(&mut popup, &cfg);
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
    open(&mut popup, &cfg);

    // Find sandbox.enabled by key rather than hardcoded index so the test
    // survives new Conversation/LLM entries being inserted above Security.
    let sandbox_idx = popup
        .entries
        .iter()
        .position(|e| e.key == "sandbox.enabled")
        .expect("sandbox.enabled entry");
    popup.selected = sandbox_idx;

    let mut cfg = Config::default();
    assert!(cfg.sandbox.enabled);

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    let result = popup.handle_key(space, &mut cfg).unwrap();
    assert!(!cfg.sandbox.enabled);
    // The toggle must emit an UpdateSetting carrying the new value so the
    // daemon persists "false", not just any action.
    match result {
        Some(AppAction::UpdateSetting { key, value }) => {
            assert_eq!(key, "sandbox.enabled");
            assert_eq!(value, "false");
        }
        Some(_) => panic!("expected UpdateSetting, got a different AppAction variant"),
        None => panic!("expected UpdateSetting, got None"),
    }
}

#[test]
fn valid_edit_applies() {
    let mut popup = SettingsPopup::new();
    let cfg = Config::default();
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "temperature")
        .expect("temperature entry");

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
    match result {
        Some(AppAction::UpdateSetting { key, value }) => {
            assert_eq!(key, "temperature");
            assert_eq!(value, "1.5");
        }
        Some(_) => panic!("expected UpdateSetting, got a different AppAction variant"),
        None => panic!("expected UpdateSetting, got None"),
    }
    assert!(matches!(popup.mode, EditMode::Browsing));
}

#[test]
fn invalid_edit_shows_error() {
    let mut popup = SettingsPopup::new();
    let cfg = Config::default();
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "temperature")
        .expect("temperature entry");
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
    // Main grew to 18 (added skills.budget_pct alongside main's hooks.enabled).
    assert!(
        popup.entries.len() >= 18,
        "entries list should not shrink — found {} entries",
        popup.entries.len()
    );

    let cfg = Config::default();
    for entry in &popup.entries {
        let val = popup.current_value(&cfg, entry.key);
        assert_ne!(val, "?", "missing current_value for {}", entry.key);
    }
}

#[test]
fn bedrock_visibility_hides_api_key_rows_and_shows_region() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("bedrock".to_string());
    let entries = visible_entries(&cfg);
    assert!(entries.iter().any(|e| e.key == "llm.bedrock_region"));
    assert!(!entries.iter().any(|e| e.key == "llm.api_key"));
    assert!(!entries.iter().any(|e| e.key == "llm.api_key_env"));
}

#[test]
fn api_key_providers_hide_bedrock_region_row() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    let entries = visible_entries(&cfg);
    assert!(!entries.iter().any(|e| e.key == "llm.bedrock_region"));
    assert!(entries.iter().any(|e| e.key == "llm.api_key"));
    assert!(entries.iter().any(|e| e.key == "llm.api_key_env"));
}

#[test]
fn azure_openai_visibility_shows_azure_rows_and_hides_bedrock_region() {
    // Provider-gated visibility for the three Azure-only settings.
    // Showing them under non-Azure providers would clutter the popup
    // with always-blank rows; hiding them under Azure would force
    // operators to drop to the CLI to configure the provider.
    let mut cfg = Config::default();
    cfg.llm.provider = Some("azure-openai".to_string());
    let entries = visible_entries(&cfg);
    assert!(entries.iter().any(|e| e.key == "llm.azure_resource"));
    assert!(entries.iter().any(|e| e.key == "llm.azure_deployment"));
    assert!(entries.iter().any(|e| e.key == "llm.azure_api_version"));
    assert!(!entries.iter().any(|e| e.key == "llm.bedrock_region"));
    // Azure uses an API key (in v1 — AAD lands later), so the standard
    // key rows must still show.
    assert!(entries.iter().any(|e| e.key == "llm.api_key"));
}

#[test]
fn non_azure_providers_hide_azure_rows() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openai".to_string());
    let entries = visible_entries(&cfg);
    assert!(!entries.iter().any(|e| e.key == "llm.azure_resource"));
    assert!(!entries.iter().any(|e| e.key == "llm.azure_deployment"));
    assert!(!entries.iter().any(|e| e.key == "llm.azure_api_version"));
}

#[test]
fn esc_during_edit_cancels() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "sandbox.enabled")
        .expect("sandbox.enabled entry");
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
    open(&mut popup, &cfg);

    // Provider is entry 0
    popup.selected = 0;
    assert_eq!(popup.entries[0].key, "provider");
    let initial_provider_index = popup.provider_index;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    let result = popup.handle_key(right, &mut cfg).unwrap();
    // Cycling the provider persists the new provider id.
    match result {
        Some(AppAction::UpdateSetting { key, value }) => {
            assert_eq!(key, "provider");
            assert_eq!(
                value,
                PROVIDERS[(initial_provider_index + 1) % PROVIDERS.len()].0
            );
        }
        Some(_) => panic!("expected UpdateSetting, got a different AppAction variant"),
        None => panic!("expected UpdateSetting, got None"),
    }
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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "temperature")
        .expect("temperature entry");
    let original = cfg.llm.temperature;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    let result = popup.handle_key(right, &mut cfg).unwrap();
    assert!((cfg.llm.temperature - (original + 0.1)).abs() < 0.01);
    // The persisted payload must be the stepped value, not the original.
    match result {
        Some(AppAction::UpdateSetting { key, value }) => {
            assert_eq!(key, "temperature");
            let parsed: f32 = value.parse().expect("numeric temperature payload");
            assert!(
                (parsed - (original + 0.1)).abs() < 0.01,
                "payload was {value}"
            );
        }
        Some(_) => panic!("expected UpdateSetting, got a different AppAction variant"),
        None => panic!("expected UpdateSetting, got None"),
    }
}

#[test]
fn float_step_left_decreases() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "temperature")
        .expect("temperature entry");
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
    open(&mut popup, &cfg);

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
    cfg.llm.model = "claude-haiku-4-5".to_string();
    open(&mut popup, &cfg);

    assert_eq!(popup.provider_index, 2); // anthropic is index 2
    assert_eq!(popup.model_index, 2); // claude-haiku-4-5 is index 2 in ANTHROPIC_MODELS
}

#[test]
fn float_clamps_at_max() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    cfg.llm.temperature = 2.0;
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

    // Start at openrouter (index 0), cycle to openai (index 1)
    popup.selected = 0;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    popup.handle_key(right, &mut cfg).unwrap();

    // Config should now have openai provider and first openai model
    assert_eq!(cfg.llm.provider.as_deref(), Some("openai"));
    assert_eq!(cfg.llm.model, "gpt-5");
}

#[test]
fn model_right_opens_selection() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

    // Select second model in current provider's list
    popup.mode = EditMode::SelectingModel {
        selected: 1,
        scroll_offset: 0,
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let result = popup.handle_key(enter, &mut cfg).unwrap();

    assert!(matches!(popup.mode, EditMode::Browsing));
    assert_eq!(popup.model_index, 1);

    let provider_id = PROVIDERS[popup.provider_index].0;
    let models = models_for_provider(provider_id);
    assert_eq!(cfg.llm.model, models[1].0);
    match result {
        Some(AppAction::UpdateSetting { key, value }) => {
            assert_eq!(key, "model");
            assert_eq!(value, models[1].0);
        }
        Some(_) => panic!("expected UpdateSetting, got a different AppAction variant"),
        None => panic!("expected UpdateSetting, got None"),
    }
}

#[test]
fn model_selection_esc_cancels() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    let original_model = cfg.llm.model.clone();
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "conversation.max_iterations")
        .expect("conversation.max_iterations entry");

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    let result = popup.handle_key(right, &mut cfg).unwrap();
    assert!(result.is_none());
}

#[test]
fn bool_current_value_reflects_config_state() {
    let popup = SettingsPopup::new();
    let mut cfg = Config::default();

    // sandbox.enabled defaults to true and current_value must mirror it.
    assert!(cfg.sandbox.enabled);
    assert_eq!(popup.current_value(&cfg, "sandbox.enabled"), "true");

    // Flipping the config flips the reported value — guards against a
    // stale/hardcoded display string.
    cfg.sandbox.enabled = false;
    assert_eq!(popup.current_value(&cfg, "sandbox.enabled"), "false");
}

#[test]
fn display_value_no_source_tags() {
    let popup = SettingsPopup::new();
    let cfg = Config::default();

    // Verify no current_value contains [db] or [toml] — those are gone
    for entry in &popup.entries {
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
    // No daemon snapshot loaded → `customized_keys` is empty, so every
    // setting reports `SettingSource::Default`.
    let popup = SettingsPopup::new();
    for entry in &popup.entries {
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
    open(&mut popup, &cfg);

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
    popup.handle_key(r, &mut cfg).unwrap();
    assert_eq!(popup.mode, EditMode::ConfirmReset);
}

#[test]
fn confirm_reset_cancel_on_non_y() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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
    assert!(matches!(result, Some(AppAction::ConfigReloaded)));
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
    open(&mut popup, &cfg);

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
    open(&mut popup, &cfg);

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

// The pre-migration save-failure tests opened a real read-only SQLite
// DB to force `set_setting` to return Err. With the daemon path, the
// failure mode "the wire reports failure" requires standing up a fake
// gRPC server — out of scope for the unit suite. Daemon-side failure
// surfaces are exercised by the integration tests in
// `crates/borgd/tests/admin_rpc_smoke.rs`.

// ── Secret / Text row tests ──

#[test]
fn secret_row_renders_masked_label_in_current_value() {
    let popup = SettingsPopup::new();
    let mut cfg = Config::default();
    // No api_key configured → (not set)
    let value = popup.current_value(&cfg, "llm.api_key");
    assert_eq!(value, "(not set)");

    // With an api_key SecretRef configured → (set), still never leaks raw value
    cfg.llm.api_key = Some(cortex::security::secrets_resolve::SecretRef::Env {
        var: "FAKE_KEY_VAR".to_string(),
    });
    let value = popup.current_value(&cfg, "llm.api_key");
    assert_eq!(value, "(set)");
    assert!(!value.contains("FAKE_KEY_VAR"));
}

#[test]
fn secret_enter_starts_with_empty_buffer() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    cfg.llm.api_key = Some(cortex::security::secrets_resolve::SecretRef::Env {
        var: "FAKE_KEY_VAR".to_string(),
    });
    open(&mut popup, &cfg);
    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "llm.api_key")
        .expect("llm.api_key entry");

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    popup.handle_key(enter, &mut cfg).unwrap();
    match &popup.mode {
        EditMode::Editing { buffer } => {
            assert!(
                buffer.is_empty(),
                "secret edit should start with empty buffer, got: {buffer}"
            );
        }
        other => panic!("expected Editing, got {other:?}"),
    }
}

#[test]
fn secret_buffer_retains_literal_chars_while_editing() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);
    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "llm.api_key")
        .expect("llm.api_key entry");

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    popup.handle_key(enter, &mut cfg).unwrap();
    for c in "sk-test".chars() {
        let ev = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        popup.handle_key(ev, &mut cfg).unwrap();
    }
    match &popup.mode {
        EditMode::Editing { buffer } => {
            // Buffer holds the literal key; masking happens only in render.
            assert_eq!(buffer, "sk-test");
        }
        other => panic!("expected Editing, got {other:?}"),
    }
}

#[test]
fn text_row_enter_seeds_buffer_with_raw_value() {
    let mut popup = SettingsPopup::new();
    let mut cfg = Config::default();
    open(&mut popup, &cfg);
    popup.selected = popup
        .entries
        .iter()
        .position(|e| e.key == "llm.api_key_env")
        .expect("llm.api_key_env entry");

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    popup.handle_key(enter, &mut cfg).unwrap();
    match &popup.mode {
        EditMode::Editing { buffer } => {
            assert_eq!(buffer, "OPENROUTER_API_KEY");
        }
        other => panic!("expected Editing, got {other:?}"),
    }
}

/// Guards the global slash-interceptor's gating predicate. `/` is a valid
/// character inside many setting values (paths, URLs, `OPENROUTER_API_KEY`-
/// style env names with no slashes are fine, but values like
/// `~/.config/foo` or regex blocked_paths are not). The interceptor must
/// not fire while the popup is collecting that input.
#[test]
fn wants_text_input_only_in_editing_mode() {
    use super::super::app::PopupHandler;
    let mut popup = SettingsPopup::new();
    let cfg = Config::default();
    open(&mut popup, &cfg);
    assert!(
        !popup.wants_text_input(),
        "Browsing list of settings does not consume free text"
    );
    popup.force_editing_for_test("/etc/borg".to_string());
    assert!(
        popup.wants_text_input(),
        "Editing a value buffer must claim text input so `/` is preserved"
    );
}
