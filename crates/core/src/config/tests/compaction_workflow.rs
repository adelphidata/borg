use super::*;

// ── CompactionConfig tests ──

#[test]
fn compaction_config_default_has_no_overrides() {
    let cfg = CompactionConfig::default();
    assert!(!cfg.has_overrides());
    assert!(cfg.provider.is_none());
    assert!(cfg.model.is_none());
    assert!(cfg.api_key_env.is_none());
    assert!(cfg.temperature.is_none());
    assert!(cfg.max_tokens.is_none());
    assert!(cfg.timeout_ms.is_none());
}

#[test]
fn compaction_config_has_overrides_when_model_set() {
    let cfg = CompactionConfig {
        model: Some("anthropic/claude-haiku-4-5".to_string()),
        ..Default::default()
    };
    assert!(cfg.has_overrides());
}

#[test]
fn compaction_config_has_overrides_when_provider_set() {
    let cfg = CompactionConfig {
        provider: Some("openrouter".to_string()),
        ..Default::default()
    };
    assert!(cfg.has_overrides());
}

#[test]
fn with_compaction_overrides_no_overrides_returns_same() {
    let cfg = Config::default();
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.model, cfg.llm.model);
    assert_eq!(result.llm.temperature, cfg.llm.temperature);
}

#[test]
fn with_compaction_overrides_applies_model() {
    let mut cfg = Config::default();
    cfg.compaction.model = Some("fast-model".to_string());
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.model, "fast-model");
    // Original config should be unchanged
    assert_ne!(cfg.llm.model, "fast-model");
}

#[test]
fn with_compaction_overrides_applies_provider() {
    let mut cfg = Config::default();
    cfg.compaction.provider = Some("openai".to_string());
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.provider, Some("openai".to_string()));
}

#[test]
fn with_compaction_overrides_applies_temperature() {
    let mut cfg = Config::default();
    cfg.compaction.temperature = Some(0.2);
    let result = cfg.with_compaction_overrides();
    assert!((result.llm.temperature - 0.2).abs() < f32::EPSILON);
}

#[test]
fn with_compaction_overrides_applies_max_tokens() {
    let mut cfg = Config::default();
    cfg.compaction.max_tokens = Some(2048);
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.max_tokens, 2048);
}

#[test]
fn with_compaction_overrides_applies_api_key_env() {
    let mut cfg = Config::default();
    cfg.compaction.api_key_env = Some("COMPACTION_KEY".to_string());
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.api_key_env, "COMPACTION_KEY");
}

#[test]
fn with_compaction_overrides_applies_timeout() {
    let mut cfg = Config::default();
    cfg.compaction.timeout_ms = Some(90000);
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.request_timeout_ms, 90000);
}

#[test]
fn with_compaction_overrides_applies_all() {
    let mut cfg = Config::default();
    cfg.compaction = CompactionConfig {
        provider: Some("openrouter".to_string()),
        model: Some("anthropic/claude-haiku-4-5".to_string()),
        api_key_env: Some("COMPACTION_KEY".to_string()),
        temperature: Some(0.3),
        max_tokens: Some(1024),
        timeout_ms: Some(60000),
    };
    let result = cfg.with_compaction_overrides();
    assert_eq!(result.llm.provider, Some("openrouter".to_string()));
    assert_eq!(result.llm.model, "anthropic/claude-haiku-4-5");
    assert_eq!(result.llm.api_key_env, "COMPACTION_KEY");
    assert!((result.llm.temperature - 0.3).abs() < f32::EPSILON);
    assert_eq!(result.llm.max_tokens, 1024);
    assert_eq!(result.llm.request_timeout_ms, 60000);
}

#[test]
fn compaction_config_serde() {
    let toml_str = r#"
[compaction]
provider = "openrouter"
model = "anthropic/claude-haiku-4-5"
temperature = 0.3
max_tokens = 1024
timeout_ms = 60000
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.compaction.provider, Some("openrouter".to_string()));
    assert_eq!(
        cfg.compaction.model,
        Some("anthropic/claude-haiku-4-5".to_string())
    );
    assert!((cfg.compaction.temperature.unwrap() - 0.3).abs() < f32::EPSILON);
    assert_eq!(cfg.compaction.max_tokens, Some(1024));
    assert_eq!(cfg.compaction.timeout_ms, Some(60000));
}

#[test]
fn compaction_config_empty_serde() {
    let toml_str = "";
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert!(!cfg.compaction.has_overrides());
}

#[test]
fn apply_setting_workflow_enabled_valid_values() {
    let mut cfg = Config::default();

    let result = cfg.apply_setting("workflow.enabled", "on").unwrap();
    assert!(result.contains("on"));
    assert_eq!(cfg.workflow.enabled, "on");

    let result = cfg.apply_setting("workflow.enabled", "off").unwrap();
    assert!(result.contains("off"));
    assert_eq!(cfg.workflow.enabled, "off");

    let result = cfg.apply_setting("workflow.enabled", "auto").unwrap();
    assert!(result.contains("auto"));
    assert_eq!(cfg.workflow.enabled, "auto");
}

#[test]
fn apply_setting_workflow_enabled_invalid_value() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("workflow.enabled", "maybe").is_err());
    assert!(cfg.apply_setting("workflow.enabled", "true").is_err());
    assert!(cfg.apply_setting("workflow.enabled", "").is_err());
}

#[test]
fn workflow_config_default_is_auto() {
    let cfg = Config::default();
    assert_eq!(cfg.workflow.enabled, "auto");
}

#[test]
fn workflow_config_serde_roundtrip() {
    let toml_str = r#"
[workflow]
enabled = "off"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.workflow.enabled, "off");
}
