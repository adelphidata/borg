use super::*;
#[allow(unused_imports)]
use crate::secrets_resolve::SecretRef;
use std::io::Write;

#[test]
fn default_config_values() {
    let cfg = Config::default();
    assert!(cfg.llm.provider.is_none());
    assert_eq!(cfg.llm.api_key_env, "OPENROUTER_API_KEY");
    assert_eq!(cfg.llm.model, "");
    assert!((cfg.llm.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(cfg.llm.max_tokens, 4096);
    assert!(cfg.llm.base_url.is_none());
    assert_eq!(cfg.heartbeat.interval, "30m");
    assert_eq!(cfg.tools.default_timeout_ms, 30000);
    assert!(cfg.sandbox.enabled);
    assert_eq!(cfg.sandbox.mode, "strict");
    assert_eq!(cfg.memory.max_context_tokens, 8000);
    assert!(cfg.skills.enabled);
    assert_eq!(cfg.skills.max_context_tokens, 4000);
    assert_eq!(cfg.conversation.max_history_tokens, 32000);
    assert_eq!(cfg.conversation.max_iterations, 25);
    assert!(cfg.conversation.show_thinking);
    assert!(cfg.security.secret_detection);
}

#[test]
fn parse_complete_config_toml() {
    let toml_str = r#"
[llm]
api_key_env = "MY_KEY"
model = "openai/gpt-4"
temperature = 0.9
max_tokens = 2048

[heartbeat]
interval = "1h"
quiet_hours_start = "22:00"
quiet_hours_end = "08:00"
cron = "0 */2 * * *"

[tools]
default_timeout_ms = 60000

[sandbox]
enabled = false
mode = "permissive"

[memory]
max_context_tokens = 16000

[conversation]
max_history_tokens = 64000
max_iterations = 50
show_thinking = false

[security]
secret_detection = false
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse complete toml");
    assert_eq!(cfg.llm.api_key_env, "MY_KEY");
    assert_eq!(cfg.llm.model, "openai/gpt-4");
    assert_eq!(cfg.conversation.max_iterations, 50);
    assert!(!cfg.conversation.show_thinking);
    assert!(!cfg.security.secret_detection);
}

#[test]
fn parse_empty_config_toml_yields_defaults() {
    let cfg: Config = toml::from_str("").expect("should parse empty toml");
    let defaults = Config::default();
    assert_eq!(cfg.llm.api_key_env, defaults.llm.api_key_env);
    assert_eq!(cfg.llm.model, defaults.llm.model);
    assert_eq!(
        cfg.conversation.max_iterations,
        defaults.conversation.max_iterations
    );
}

#[test]
fn parse_minimal_config_toml_with_partial_sections() {
    let toml_str = r#"
[llm]
model = "meta/llama-3"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse partial toml");
    assert_eq!(cfg.llm.model, "meta/llama-3");
    assert_eq!(cfg.llm.api_key_env, "OPENROUTER_API_KEY");
}

#[test]
fn load_from_nonexistent_path_returns_defaults() {
    let path = Path::new("/tmp/borg_test_nonexistent_config.toml");
    let cfg = Config::load_from(path).expect("should return default for missing file");
    let defaults = Config::default();
    assert_eq!(cfg.llm.model, defaults.llm.model);
}

#[test]
fn load_from_file_on_disk() {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path().join("config.toml");
    {
        let mut f = std::fs::File::create(&path).expect("create file");
        write!(
            f,
            "[llm]\nmodel = \"test-model\"\n[sandbox]\nenabled = false\n"
        )
        .expect("write file");
    }
    let cfg = Config::load_from(&path).expect("should load from temp file");
    assert_eq!(cfg.llm.model, "test-model");
    assert!(!cfg.sandbox.enabled);
}

#[test]
fn api_key_resolved_from_env() {
    let env_name = "BORG_TEST_API_KEY_RESOLVE";
    let mut cfg = Config::default();
    cfg.llm.api_key_env = env_name.to_string();
    std::env::remove_var(env_name);
    assert!(cfg.api_key().is_err());
    std::env::set_var(env_name, "sk-test-12345");
    let key = cfg.api_key().expect("should resolve key from env");
    assert_eq!(key, "sk-test-12345");
    std::env::remove_var(env_name);
}

#[test]
fn parse_config_with_user_section() {
    let toml_str = r#"
[user]
name = "Mike"
agent_name = "Buddy"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.user.name.as_deref(), Some("Mike"));
    assert_eq!(cfg.user.agent_name.as_deref(), Some("Buddy"));
}

#[test]
fn apply_setting_model() {
    let mut cfg = Config::default();
    let result = cfg.apply_setting("model", "gpt-4o").unwrap();
    assert!(result.contains("gpt-4o"));
    assert_eq!(cfg.llm.model, "gpt-4o");
}

#[test]
fn apply_setting_temperature() {
    let mut cfg = Config::default();
    cfg.apply_setting("temperature", "0.5").unwrap();
    assert!((cfg.llm.temperature - 0.5).abs() < f32::EPSILON);
}

#[test]
fn apply_setting_temperature_out_of_range() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("temperature", "3.0").is_err());
    assert!(cfg.apply_setting("temperature", "-1.0").is_err());
}

#[test]
fn apply_setting_max_iterations() {
    let mut cfg = Config::default();
    cfg.apply_setting("conversation.max_iterations", "50")
        .unwrap();
    assert_eq!(cfg.conversation.max_iterations, 50);
}

#[test]
fn apply_setting_show_thinking() {
    let mut cfg = Config::default();
    cfg.apply_setting("conversation.show_thinking", "false")
        .unwrap();
    assert!(!cfg.conversation.show_thinking);
}

#[test]
fn apply_setting_secret_detection() {
    let mut cfg = Config::default();
    cfg.apply_setting("security.secret_detection", "false")
        .unwrap();
    assert!(!cfg.security.secret_detection);
}

#[test]
fn apply_setting_unknown_key_errors() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("nonexistent", "value").is_err());
}

#[test]
fn apply_setting_max_tokens_rejects_zero() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("max_tokens", "0").is_err());
}

#[test]
fn apply_setting_memory_max_context_tokens_rejects_zero() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("memory.max_context_tokens", "0").is_err());
}

#[test]
fn apply_setting_sandbox_mode_rejects_invalid() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("sandbox.mode", "bogus").is_err());
    assert!(cfg.apply_setting("sandbox.mode", "strict").is_ok());
    assert!(cfg.apply_setting("sandbox.mode", "permissive").is_ok());
}

#[test]
fn display_settings_contains_key_fields() {
    let cfg = Config::default();
    let display = cfg.display_settings();
    assert!(display.contains("model"));
    assert!(display.contains("max_iterations"));
    assert!(display.contains("secret_detection"));
}

#[test]
fn default_web_config_values() {
    let cfg = WebConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.search_provider, "auto");
    assert!(cfg.search_api_key_env.is_none());
}

#[test]
fn default_tasks_config_values() {
    let cfg = TasksConfig::default();
    assert_eq!(cfg.max_concurrent, 3);
}

#[test]
fn default_llm_config_retry_fields() {
    let cfg = LlmConfig::default();
    assert_eq!(cfg.max_retries, 3);
    assert_eq!(cfg.initial_retry_delay_ms, 200);
    assert_eq!(cfg.request_timeout_ms, 120_000);
}

#[test]
fn path_helpers() {
    let data = Config::data_dir().unwrap();
    assert!(data.to_string_lossy().ends_with(".borg"));

    let memory = Config::memory_dir().unwrap();
    assert_eq!(memory, data.join("memory"));

    let skills = Config::skills_dir().unwrap();
    assert_eq!(skills, data.join("skills"));

    let logs = Config::logs_dir().unwrap();
    assert_eq!(logs, data.join("logs"));

    let sessions = Config::sessions_dir().unwrap();
    assert_eq!(sessions, data.join("sessions"));

    let db = Config::db_path().unwrap();
    assert_eq!(db, data.join("borg.db"));

    let identity = Config::identity_path().unwrap();
    assert_eq!(identity, data.join("IDENTITY.md"));

    let mem_index = Config::memory_index_path().unwrap();
    assert_eq!(mem_index, data.join("MEMORY.md"));
}

#[test]
fn partial_section_defaults_remaining_fields() {
    // Only set model; temperature, max_tokens, etc. should get defaults
    let toml_str = r#"
[llm]
model = "custom-model"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.model, "custom-model");
    assert!((cfg.llm.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(cfg.llm.max_tokens, 4096);
    assert_eq!(cfg.llm.max_retries, 3);
    assert_eq!(cfg.llm.initial_retry_delay_ms, 200);
    assert_eq!(cfg.llm.request_timeout_ms, 120_000);
    assert_eq!(cfg.llm.api_key_env, "OPENROUTER_API_KEY");
    assert!(cfg.llm.base_url.is_none());
}

#[test]
fn partial_web_config_defaults_remaining_fields() {
    let toml_str = r#"
[web]
enabled = false
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(!cfg.web.enabled);
    assert_eq!(cfg.web.search_provider, "auto");
    assert!(cfg.web.search_api_key_env.is_none());
}

#[test]
fn partial_tasks_config_defaults_remaining_fields() {
    let toml_str = r#"
[tasks]
max_concurrent = 5
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.tasks.max_concurrent, 5);
}

#[test]
fn save_and_reload_round_trip() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let config_path = dir.path().join("config.toml");
    let mut cfg = Config::default();
    cfg.llm.model = "test-round-trip".to_string();
    cfg.llm.temperature = 1.5;
    let content = toml::to_string_pretty(&cfg).expect("serialize");
    std::fs::write(&config_path, content).expect("write");
    let loaded = Config::load_from(&config_path).expect("reload");
    assert_eq!(loaded.llm.model, "test-round-trip");
    assert!((loaded.llm.temperature - 1.5).abs() < f32::EPSILON);
}

#[test]
fn default_budget_config_values() {
    let cfg = BudgetConfig::default();
    assert_eq!(cfg.monthly_token_limit, 1_000_000);
    assert!((cfg.warning_threshold - 0.8).abs() < f64::EPSILON);
}

#[test]
fn parse_budget_config_from_toml() {
    let toml_str = r#"
[budget]
monthly_token_limit = 5000000
warning_threshold = 0.9
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.budget.monthly_token_limit, 5_000_000);
    assert!((cfg.budget.warning_threshold - 0.9).abs() < f64::EPSILON);
}

#[test]
fn parse_budget_config_defaults_when_absent() {
    let cfg: Config = toml::from_str("").expect("should parse");
    assert_eq!(cfg.budget.monthly_token_limit, 1_000_000);
    assert!((cfg.budget.warning_threshold - 0.8).abs() < f64::EPSILON);
}

#[test]
fn apply_setting_budget_monthly_token_limit() {
    let mut cfg = Config::default();
    cfg.apply_setting("budget.monthly_token_limit", "1000000")
        .expect("should succeed");
    assert_eq!(cfg.budget.monthly_token_limit, 1_000_000);
}

#[test]
fn apply_setting_budget_warning_threshold() {
    let mut cfg = Config::default();
    cfg.apply_setting("budget.warning_threshold", "0.9")
        .expect("should succeed");
    assert!((cfg.budget.warning_threshold - 0.9).abs() < f64::EPSILON);
}

#[test]
fn apply_setting_budget_warning_threshold_out_of_range() {
    let mut cfg = Config::default();
    assert!(cfg
        .apply_setting("budget.warning_threshold", "1.5")
        .is_err());
    assert!(cfg
        .apply_setting("budget.warning_threshold", "-0.1")
        .is_err());
}

#[test]
fn display_settings_contains_budget() {
    let cfg = Config::default();
    let display = cfg.display_settings();
    assert!(display.contains("budget.monthly_token_limit"));
    assert!(display.contains("budget.warning_threshold"));
}

#[test]
fn telemetry_config_defaults() {
    let cfg = TelemetryConfig::default();
    assert!(!cfg.tracing_enabled);
    assert!(!cfg.metrics_enabled);
    assert_eq!(cfg.otlp_endpoint, "http://localhost:4317");
    assert_eq!(cfg.service_name, "borg");
    assert!((cfg.sampling_ratio - 1.0).abs() < f64::EPSILON);
}

#[test]
fn telemetry_config_parsing() {
    let toml_str = r#"
[telemetry]
tracing_enabled = true
metrics_enabled = true
otlp_endpoint = "http://otel:4317"
service_name = "my-borg"
sampling_ratio = 0.5
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.telemetry.tracing_enabled);
    assert!(cfg.telemetry.metrics_enabled);
    assert_eq!(cfg.telemetry.otlp_endpoint, "http://otel:4317");
    assert_eq!(cfg.telemetry.service_name, "my-borg");
    assert!((cfg.telemetry.sampling_ratio - 0.5).abs() < f64::EPSILON);
}

#[test]
fn apply_setting_telemetry_hidden_from_user() {
    let mut cfg = Config::default();
    assert!(cfg
        .apply_setting("telemetry.tracing_enabled", "true")
        .is_err());
}
