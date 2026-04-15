use super::*;
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

#[test]
fn parse_config_with_secret_ref_env() {
    let toml_str = r#"
[llm]
provider = "openrouter"
api_key = { source = "env", var = "MY_SECRET_KEY" }
model = "anthropic/claude-sonnet-4"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.llm.api_key.is_some());
    if let Some(SecretRef::Env { var }) = &cfg.llm.api_key {
        assert_eq!(var, "MY_SECRET_KEY");
    } else {
        panic!("expected Env variant");
    }
}

#[test]
fn parse_config_with_secret_ref_exec() {
    let toml_str = r#"
[llm]
provider = "openrouter"
api_key = { source = "exec", command = "security", args = ["find-generic-password", "-s", "borg", "-w"] }
model = "anthropic/claude-sonnet-4"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.llm.api_key.is_some());
    if let Some(SecretRef::Exec { command, args }) = &cfg.llm.api_key {
        assert_eq!(command, "security");
        assert_eq!(args.len(), 4);
    } else {
        panic!("expected Exec variant");
    }
}

#[test]
fn parse_config_with_api_keys_list() {
    let toml_str = r#"
[llm]
provider = "openrouter"
model = "anthropic/claude-sonnet-4"

[[llm.api_keys]]
source = "env"
var = "PRIMARY_KEY"

[[llm.api_keys]]
source = "env"
var = "FALLBACK_KEY"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.api_keys.len(), 2);
}

#[test]
fn parse_config_without_secret_ref_uses_defaults() {
    let toml_str = r#"
[llm]
api_key_env = "MY_KEY"
model = "test-model"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.llm.api_key.is_none());
    assert!(cfg.llm.api_keys.is_empty());
    assert_eq!(cfg.llm.api_key_env, "MY_KEY");
}

#[test]
fn resolve_provider_prefers_secret_ref() {
    let env_name = "BORG_TEST_SECRET_REF_RESOLVE";
    std::env::set_var(env_name, "secret-ref-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    cfg.llm.api_key = Some(SecretRef::Env {
        var: env_name.to_string(),
    });
    let (provider, key) = cfg.resolve_provider().expect("should resolve");
    assert_eq!(key, "secret-ref-key");
    assert_eq!(provider, Provider::OpenRouter);
    std::env::remove_var(env_name);
}

#[test]
fn resolve_api_keys_multi() {
    let env1 = "BORG_TEST_MULTI_KEY_1";
    let env2 = "BORG_TEST_MULTI_KEY_2";
    std::env::set_var(env1, "key-one");
    std::env::set_var(env2, "key-two");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    cfg.llm.api_keys = vec![
        SecretRef::Env {
            var: env1.to_string(),
        },
        SecretRef::Env {
            var: env2.to_string(),
        },
    ];
    let (_, keys) = cfg.resolve_api_keys().expect("should resolve");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], "key-one");
    assert_eq!(keys[1], "key-two");
    std::env::remove_var(env1);
    std::env::remove_var(env2);
}

/// Ensure that serializing a Config and parsing it back produces valid TOML.
/// This catches issues like duplicate table headers.
#[test]
fn save_produces_parseable_toml() {
    let cfg = Config::default();
    let serialized = toml::to_string_pretty(&cfg).expect("serialize default config");
    let _parsed: Config = toml::from_str(&serialized)
        .unwrap_or_else(|e| panic!("default config round-trip failed:\n{serialized}\nerror: {e}"));
}

/// Same as above but with various fields populated.
#[test]
fn save_with_populated_fields_produces_parseable_toml() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    cfg.llm.model = "anthropic/claude-sonnet-4".to_string();
    cfg.llm.api_key = Some(SecretRef::Env {
        var: "MY_KEY".to_string(),
    });
    cfg.llm.api_keys = vec![
        SecretRef::Env {
            var: "KEY1".to_string(),
        },
        SecretRef::Exec {
            command: "security".to_string(),
            args: vec!["find-generic-password".to_string(), "-w".to_string()],
        },
    ];
    cfg.user.name = Some("Test".to_string());
    cfg.user.agent_name = Some("Buddy".to_string());
    cfg.credentials.insert(
        "test".to_string(),
        CredentialValue::EnvVar("value".to_string()),
    );
    cfg.budget.monthly_token_limit = 1_000_000;

    let serialized = toml::to_string_pretty(&cfg).expect("serialize");
    let parsed: Config = toml::from_str(&serialized).unwrap_or_else(|e| {
        panic!("populated config round-trip failed:\n{serialized}\nerror: {e}")
    });
    assert_eq!(parsed.llm.model, "anthropic/claude-sonnet-4");
    assert!(parsed.llm.api_key.is_some());
    assert_eq!(parsed.llm.api_keys.len(), 2);
    assert_eq!(parsed.budget.monthly_token_limit, 1_000_000);
}

/// Verify that a realistic config.toml (matching the format produced by onboarding) parses.
#[test]
fn parse_realistic_config_toml() {
    let toml_str = r#"
[user]
name = "Mike"
agent_name = "Buddy"

[llm]
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
model = "anthropic/claude-sonnet-4"
temperature = 0.7
max_tokens = 4096
max_retries = 3
initial_retry_delay_ms = 200
request_timeout_ms = 120000

[heartbeat]
interval = "30m"

[tools]
default_timeout_ms = 30000

[sandbox]
enabled = true
mode = "strict"

[memory]
max_context_tokens = 8000

[skills]
enabled = true
max_context_tokens = 4000

[conversation]
max_history_tokens = 32000
max_iterations = 25
show_thinking = true

[policy]
auto_approve = []
deny = []

[debug]
llm_logging = false

[security]
secret_detection = true
blocked_paths = [".ssh", ".aws", ".gnupg", ".config/gh", ".env", "credentials", "private_key"]

[web]
enabled = true
search_provider = "duckduckgo"

[tasks]
enabled = false
max_concurrent = 3

[budget]
monthly_token_limit = 0
warning_threshold = 0.8

[gateway]
enabled = false
host = "127.0.0.1"
port = 7842
max_concurrent = 10
request_timeout_ms = 120000

[credentials]
"#;
    let cfg: Config =
        toml::from_str(toml_str).unwrap_or_else(|e| panic!("realistic config parse failed: {e}"));
    assert_eq!(cfg.user.name.as_deref(), Some("Mike"));
    assert_eq!(cfg.llm.model, "anthropic/claude-sonnet-4");
    assert_eq!(cfg.llm.api_key_env, "OPENROUTER_API_KEY");
    assert!(cfg.llm.api_key.is_none());
}

#[test]
fn parse_credentials_legacy_string() {
    let toml_str = r#"
[credentials]
JIRA_API_TOKEN = "JIRA_API_TOKEN"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.credentials.contains_key("JIRA_API_TOKEN"));
    if let CredentialValue::EnvVar(var) = &cfg.credentials["JIRA_API_TOKEN"] {
        assert_eq!(var, "JIRA_API_TOKEN");
    } else {
        panic!("expected EnvVar variant for legacy string");
    }
}

#[test]
fn parse_credentials_secret_ref() {
    let toml_str = r#"
[credentials]
SLACK_TOKEN = { source = "exec", command = "echo", args = ["slack-secret"] }
GH_TOKEN = { source = "file", path = "/tmp/token" }
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.credentials.contains_key("SLACK_TOKEN"));
    assert!(cfg.credentials.contains_key("GH_TOKEN"));
    if let CredentialValue::Ref(SecretRef::Exec { command, .. }) = &cfg.credentials["SLACK_TOKEN"] {
        assert_eq!(command, "echo");
    } else {
        panic!("expected Ref(Exec) variant");
    }
}

#[test]
fn resolve_credentials_filters_failures() {
    let var_name = "BORG_TEST_CRED_GOOD";
    std::env::set_var(var_name, "good-value");
    let mut cfg = Config::default();
    cfg.credentials.insert(
        "GOOD".to_string(),
        CredentialValue::EnvVar(var_name.to_string()),
    );
    cfg.credentials.insert(
        "BAD".to_string(),
        CredentialValue::EnvVar("DEFINITELY_NOT_SET_XYZ_12345".to_string()),
    );
    let resolved = cfg.resolve_credentials();
    assert_eq!(resolved.get("GOOD").unwrap(), "good-value");
    assert!(!resolved.contains_key("BAD"));
    std::env::remove_var(var_name);
}

#[test]
fn credential_value_round_trip() {
    let mut cfg = Config::default();
    cfg.credentials.insert(
        "LEGACY".to_string(),
        CredentialValue::EnvVar("MY_VAR".to_string()),
    );
    cfg.credentials.insert(
        "EXEC_CRED".to_string(),
        CredentialValue::Ref(SecretRef::Exec {
            command: "security".to_string(),
            args: vec!["find-generic-password".to_string(), "-w".to_string()],
        }),
    );
    let serialized = toml::to_string_pretty(&cfg).expect("serialize");
    let parsed: Config = toml::from_str(&serialized).expect("deserialize");
    assert!(parsed.credentials.contains_key("LEGACY"));
    assert!(parsed.credentials.contains_key("EXEC_CRED"));
}

#[test]
fn save_round_trip_no_duplicate_credentials() {
    // Simulate the plugin install flow: load config, add a keychain credential, save.
    // Verify the serialized output is valid TOML (no duplicate [credentials] section).
    let mut cfg = Config::default();
    cfg.llm.model = "test-model".to_string();
    cfg.credentials.insert(
        "TELEGRAM_BOT_TOKEN".to_string(),
        CredentialValue::Ref(SecretRef::Keychain {
            service: "borg-messaging-telegram".to_string(),
            account: "borg-TELEGRAM_BOT_TOKEN".to_string(),
        }),
    );
    let serialized = toml::to_string_pretty(&cfg).expect("serialize");
    // Must be valid TOML on re-parse
    let reparsed: Config = toml::from_str(&serialized)
        .unwrap_or_else(|e| panic!("serialized config is invalid TOML: {e}\n---\n{serialized}"));
    assert!(reparsed.credentials.contains_key("TELEGRAM_BOT_TOKEN"));

    // No duplicate [credentials] header
    let count = serialized
        .lines()
        .filter(|l| l.trim() == "[credentials]")
        .count();
    assert!(
        count <= 1,
        "expected at most 1 [credentials] section, got {count}\n---\n{serialized}"
    );
}

#[test]
fn save_round_trip_with_existing_credentials_section() {
    // Reproduce the bug: config already has an empty [credentials] section,
    // then we load, add a credential, and re-serialize.
    let original = r#"
[llm]
model = "test"

[credentials]
"#;
    let mut cfg: Config = toml::from_str(original).expect("parse original");
    cfg.credentials.insert(
        "MY_KEY".to_string(),
        CredentialValue::Ref(SecretRef::Keychain {
            service: "svc".to_string(),
            account: "acct".to_string(),
        }),
    );
    let serialized = toml::to_string_pretty(&cfg).expect("serialize");
    // Must still be valid TOML
    let _reparsed: Config = toml::from_str(&serialized)
        .unwrap_or_else(|e| panic!("re-serialized config is invalid TOML: {e}\n---\n{serialized}"));

    let count = serialized
        .lines()
        .filter(|l| l.trim() == "[credentials]" || l.trim().starts_with("[credentials."))
        .count();
    assert!(
        count <= 1,
        "expected at most 1 credentials header, got {count}\n---\n{serialized}"
    );
}

#[test]
fn dedup_toml_tables_removes_duplicate_credentials() {
    let input = r#"[llm]
model = "test"

[credentials]

[credentials]
"#;
    let output = Config::dedup_toml_tables(input);
    let count = output
        .lines()
        .filter(|l| l.trim() == "[credentials]")
        .count();
    assert_eq!(
        count, 1,
        "should have exactly 1 [credentials]\n---\n{output}"
    );
    // Must parse successfully
    let _cfg: Config = toml::from_str(&output).expect("deduped config should parse");
}

#[test]
fn dedup_toml_tables_keeps_distinct_sections() {
    let input = r#"[llm]
model = "test"

[credentials]
KEY = "val"

[security]
secret_detection = true
"#;
    let output = Config::dedup_toml_tables(input);
    assert!(output.contains("[credentials]"));
    assert!(output.contains("[security]"));
    assert!(output.contains("KEY = \"val\""));
}

#[test]
fn dedup_toml_tables_drops_duplicate_content() {
    let input = r#"[gateway]
enabled = true

[gateway]
enabled = false
"#;
    let output = Config::dedup_toml_tables(input);
    let count = output.lines().filter(|l| l.trim() == "[gateway]").count();
    assert_eq!(count, 1);
    // The first occurrence's content is kept
    assert!(output.contains("enabled = true"));
    // The duplicate's content is dropped
    assert!(!output.contains("enabled = false"));
}

#[test]
fn load_from_handles_duplicate_credentials() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let config_path = dir.path().join("config.toml");
    // Write a config with duplicate [credentials] — the exact bug scenario
    std::fs::write(
        &config_path,
        r#"[llm]
model = "test"

[credentials]

[credentials]
"#,
    )
    .expect("write");
    let cfg = Config::load_from(&config_path).expect("load should succeed despite duplicates");
    assert_eq!(cfg.llm.model, "test");
}

#[test]
fn parse_credentials_secret_ref_env_variant() {
    let toml_str = r#"
[credentials]
MY_KEY = { source = "env", var = "MY_KEY_VAR" }
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    match &cfg.credentials["MY_KEY"] {
        CredentialValue::Ref(SecretRef::Env { var }) => {
            assert_eq!(var, "MY_KEY_VAR");
        }
        other => panic!("expected Ref(Env), got {other:?}"),
    }
}

#[test]
fn default_browser_config_values() {
    let cfg = BrowserConfig::default();
    assert!(cfg.enabled);
    assert!(cfg.headless);
    assert!(cfg.executable.is_none());
    assert_eq!(cfg.cdp_port, 9222);
    assert!(!cfg.no_sandbox);
    assert_eq!(cfg.timeout_ms, 30000);
    assert_eq!(cfg.startup_timeout_ms, 15000);
}

#[test]
fn parse_browser_config_toml() {
    let toml_str = r#"
[browser]
enabled = false
headless = false
executable = "/usr/bin/chromium"
cdp_port = 9333
no_sandbox = true
timeout_ms = 60000
startup_timeout_ms = 20000
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(!cfg.browser.enabled);
    assert!(!cfg.browser.headless);
    assert_eq!(cfg.browser.executable.as_deref(), Some("/usr/bin/chromium"));
    assert_eq!(cfg.browser.cdp_port, 9333);
    assert!(cfg.browser.no_sandbox);
    assert_eq!(cfg.browser.timeout_ms, 60000);
    assert_eq!(cfg.browser.startup_timeout_ms, 20000);
}

#[test]
fn parse_empty_toml_yields_browser_defaults() {
    let cfg: Config = toml::from_str("").expect("should parse");
    assert!(cfg.browser.enabled);
    assert!(cfg.browser.headless);
    assert_eq!(cfg.browser.cdp_port, 9222);
}

#[test]
fn apply_setting_browser_headless() {
    let mut cfg = Config::default();
    cfg.apply_setting("browser.headless", "false").unwrap();
    assert!(!cfg.browser.headless);
}

#[test]
fn apply_setting_browser_cdp_port_hidden() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("browser.cdp_port", "9333").is_err());
}

#[test]
fn display_settings_contains_browser() {
    let cfg = Config::default();
    let display = cfg.display_settings();
    assert!(display.contains("browser.enabled"));
    assert!(display.contains("browser.headless"));
    assert!(!display.contains("browser.cdp_port"));
}

#[test]
fn tts_config_defaults() {
    let cfg = TtsConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.models.is_empty());
    assert_eq!(cfg.default_voice, "alloy");
    assert_eq!(cfg.default_format, "mp3");
    assert_eq!(cfg.max_text_length, 4096);
    assert_eq!(cfg.timeout_ms, 30_000);
    assert!(!cfg.auto_mode);
}

#[test]
fn apply_setting_tts_enabled() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.enabled", "true").unwrap();
    assert!(cfg.tts.enabled);
}

#[test]
fn apply_setting_tts_auto_mode() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.auto_mode", "true").unwrap();
    assert!(cfg.tts.auto_mode);
}

#[test]
fn apply_setting_tts_default_voice() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.default_voice", "nova").unwrap();
    assert_eq!(cfg.tts.default_voice, "nova");
}

#[test]
fn apply_setting_tts_default_format() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.default_format", "opus").unwrap();
    assert_eq!(cfg.tts.default_format, "opus");
}

#[test]
fn apply_setting_tts_default_format_invalid() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("tts.default_format", "ogg").is_err());
}

#[test]
fn parse_tts_config() {
    let toml_str = r#"
[tts]
enabled = true
default_voice = "nova"
default_format = "opus"
auto_mode = true

[[tts.models]]
provider = "openai"
model = "tts-1"

[[tts.models]]
provider = "elevenlabs"
voice = "21m00Tcm4TlvDq8ikWAM"
api_key_env = "ELEVENLABS_API_KEY"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert!(config.tts.enabled);
    assert!(config.tts.auto_mode);
    assert_eq!(config.tts.default_voice, "nova");
    assert_eq!(config.tts.default_format, "opus");
    assert_eq!(config.tts.models.len(), 2);
    assert_eq!(config.tts.models[0].provider, "openai");
    assert_eq!(config.tts.models[0].model.as_deref(), Some("tts-1"));
    assert_eq!(config.tts.models[1].provider, "elevenlabs");
    assert_eq!(
        config.tts.models[1].voice.as_deref(),
        Some("21m00Tcm4TlvDq8ikWAM")
    );
}

#[test]
fn evolution_enabled_default_true() {
    let cfg = Config::default();
    assert!(cfg.evolution.enabled);
}

#[test]
fn apply_setting_evolution_enabled() {
    let mut cfg = Config::default();
    cfg.apply_setting("evolution.enabled", "false").unwrap();
    assert!(!cfg.evolution.enabled);
    cfg.apply_setting("evolution.enabled", "true").unwrap();
    assert!(cfg.evolution.enabled);
}

#[test]
fn apply_setting_evolution_enabled_invalid() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("evolution.enabled", "nope").is_err());
}

#[test]
fn embeddings_config_defaults() {
    let cfg = EmbeddingsConfig::default();
    assert!(cfg.enabled);
    assert!(cfg.provider.is_none());
    assert!(cfg.model.is_none());
    assert!(cfg.dimension.is_none());
    assert!(cfg.api_key_env.is_none());
    assert!((cfg.recency_weight - 0.2).abs() < f32::EPSILON);
}

#[test]
fn memory_config_includes_embeddings() {
    let cfg = MemoryConfig::default();
    assert_eq!(cfg.max_context_tokens, 8000);
    assert!(cfg.embeddings.enabled);
}

#[test]
fn embeddings_config_toml_deserialization() {
    let toml_str = r#"
[memory]
max_context_tokens = 4000

[memory.embeddings]
enabled = false
provider = "gemini"
model = "text-embedding-004"
dimension = 768
recency_weight = 0.5
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.memory.max_context_tokens, 4000);
    assert!(!cfg.memory.embeddings.enabled);
    assert_eq!(cfg.memory.embeddings.provider.as_deref(), Some("gemini"));
    assert_eq!(
        cfg.memory.embeddings.model.as_deref(),
        Some("text-embedding-004")
    );
    assert_eq!(cfg.memory.embeddings.dimension, Some(768));
    assert!((cfg.memory.embeddings.recency_weight - 0.5).abs() < f32::EPSILON);
}

#[test]
fn embeddings_config_absent_uses_defaults() {
    let toml_str = r#"
[memory]
max_context_tokens = 6000
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.memory.max_context_tokens, 6000);
    assert!(cfg.memory.embeddings.enabled);
    assert!(cfg.memory.embeddings.provider.is_none());
}

// ── Feature #11: Provider Failover config tests ──

#[test]
fn parse_llm_fallback_config() {
    let toml_str = r#"
[llm]
provider = "openrouter"
model = "anthropic/claude-sonnet-4"

[[llm.fallback]]
provider = "anthropic"
model = "claude-sonnet-4"
api_key_env = "ANTHROPIC_API_KEY"

[[llm.fallback]]
provider = "openai"
model = "gpt-4.1"
temperature = 0.5
max_tokens = 8192
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.llm.fallback.len(), 2);
    assert_eq!(cfg.llm.fallback[0].provider, "anthropic");
    assert_eq!(cfg.llm.fallback[0].model, "claude-sonnet-4");
    assert_eq!(
        cfg.llm.fallback[0].api_key_env.as_deref(),
        Some("ANTHROPIC_API_KEY")
    );
    assert_eq!(cfg.llm.fallback[1].provider, "openai");
    assert_eq!(cfg.llm.fallback[1].model, "gpt-4.1");
    assert!((cfg.llm.fallback[1].temperature.unwrap() - 0.5).abs() < f32::EPSILON);
    assert_eq!(cfg.llm.fallback[1].max_tokens, Some(8192));
}

#[test]
fn parse_no_fallback_config() {
    let toml_str = r#"
[llm]
model = "test-model"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert!(cfg.llm.fallback.is_empty());
}

// ── Feature #10: Audio config tests ──

#[test]
fn parse_audio_config() {
    let toml_str = r#"
[audio]
enabled = true
max_file_size = 20971520
min_file_size = 1024
language = "en"
timeout_ms = 60000

[[audio.models]]
provider = "openai"
model = "whisper-1"

[[audio.models]]
provider = "groq"
model = "whisper-large-v3-turbo"
api_key_env = "GROQ_API_KEY"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert!(cfg.audio.enabled);
    assert_eq!(cfg.audio.max_file_size, 20_971_520);
    assert_eq!(cfg.audio.min_file_size, 1024);
    assert_eq!(cfg.audio.language.as_deref(), Some("en"));
    assert_eq!(cfg.audio.timeout_ms, 60_000);
    assert_eq!(cfg.audio.models.len(), 2);
    assert_eq!(cfg.audio.models[0].provider, "openai");
    assert_eq!(cfg.audio.models[0].model.as_deref(), Some("whisper-1"));
    assert_eq!(cfg.audio.models[1].provider, "groq");
    assert_eq!(
        cfg.audio.models[1].api_key_env.as_deref(),
        Some("GROQ_API_KEY")
    );
}

#[test]
fn audio_config_defaults() {
    let cfg = AudioConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.models.is_empty());
    assert_eq!(cfg.max_file_size, 20 * 1024 * 1024);
    assert_eq!(cfg.min_file_size, 1024);
    assert!(!cfg.echo_transcript);
}

// ── Feature #12: Gateway bindings config tests ──

#[test]
fn parse_gateway_bindings_config() {
    let toml_str = r#"
[[gateway.bindings]]
channel = "telegram"
provider = "anthropic"
model = "claude-sonnet-4"
identity = "work-identity.md"
memory_scope = "work"

[[gateway.bindings]]
channel = "slack"
sender = "U12345*"
provider = "openai"
model = "gpt-4.1"
temperature = 0.3

[[gateway.bindings]]
channel = "discord"
peer_kind = "group"
memory_scope = "team"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.gateway.bindings.len(), 3);

    assert_eq!(cfg.gateway.bindings[0].channel, "telegram");
    assert_eq!(
        cfg.gateway.bindings[0].provider.as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        cfg.gateway.bindings[0].identity.as_deref(),
        Some("work-identity.md")
    );
    assert_eq!(
        cfg.gateway.bindings[0].memory_scope.as_deref(),
        Some("work")
    );

    assert_eq!(cfg.gateway.bindings[1].channel, "slack");
    assert_eq!(cfg.gateway.bindings[1].sender.as_deref(), Some("U12345*"));
    assert!((cfg.gateway.bindings[1].temperature.unwrap() - 0.3).abs() < f32::EPSILON);

    assert_eq!(cfg.gateway.bindings[2].channel, "discord");
    assert_eq!(cfg.gateway.bindings[2].peer_kind.as_deref(), Some("group"));
}

#[test]
fn gateway_bindings_empty_by_default() {
    let cfg = Config::default();
    assert!(cfg.gateway.bindings.is_empty());
}

// -- ToolPolicyConfig --

#[test]
fn tool_policy_default_values() {
    let policy = ToolPolicyConfig::default();
    assert_eq!(policy.profile, "full");
    assert!(policy.allow.is_empty());
    assert!(policy.deny.is_empty());
    assert!(policy.subagent_deny.contains(&"schedule".to_string()));
    assert!(policy.subagent_deny.contains(&"browser".to_string()));
}

#[test]
fn tool_policy_config_is_part_of_tools_config() {
    let cfg = Config::default();
    assert_eq!(cfg.tools.policy.profile, "full");
}

#[test]
fn parse_tool_policy_from_toml() {
    let toml_str = r#"
[tools]
default_timeout_ms = 30000

[tools.policy]
profile = "coding"
allow = ["write_memory", "group:fs"]
deny = ["security_audit"]
subagent_deny = ["manage_tasks"]
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse tools policy");
    assert_eq!(cfg.tools.policy.profile, "coding");
    assert_eq!(cfg.tools.policy.allow.len(), 2);
    assert!(cfg.tools.policy.allow.contains(&"write_memory".to_string()));
    assert!(cfg.tools.policy.allow.contains(&"group:fs".to_string()));
    assert_eq!(cfg.tools.policy.deny, vec!["security_audit".to_string()]);
    assert_eq!(
        cfg.tools.policy.subagent_deny,
        vec!["manage_tasks".to_string()]
    );
}

#[test]
fn parse_tool_policy_empty_defaults() {
    let toml_str = r#"
[tools]
default_timeout_ms = 30000
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    let default_policy = ToolPolicyConfig::default();
    assert_eq!(cfg.tools.policy.profile, default_policy.profile);
    assert_eq!(cfg.tools.policy.allow.len(), default_policy.allow.len());
    assert_eq!(cfg.tools.policy.deny.len(), default_policy.deny.len());
}

#[test]
fn test_skills_entries_deserialize() {
    let toml_str = r#"
[skills]
enabled = true
max_context_tokens = 4000

[skills.entries.slack]
enabled = true
env = { SLACK_BOT_TOKEN = "xoxb-test" }

[skills.entries.docker]
enabled = false
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert!(cfg.skills.entries.contains_key("slack"));
    assert!(cfg.skills.entries.contains_key("docker"));
    let slack = &cfg.skills.entries["slack"];
    assert!(slack.enabled);
    assert_eq!(slack.env.get("SLACK_BOT_TOKEN").unwrap(), "xoxb-test");
    let docker = &cfg.skills.entries["docker"];
    assert!(!docker.enabled);
}

#[test]
fn test_skills_entries_default_empty() {
    let cfg = SkillsConfig::default();
    assert!(cfg.entries.is_empty());
}

#[test]
fn test_skill_entry_enabled_default_true() {
    let entry = SkillEntryConfig::default();
    assert!(entry.enabled);
    assert!(entry.env.is_empty());
}

#[test]
fn parse_config_with_base_url() {
    let toml_str = r#"
[llm]
provider = "ollama"
model = "llama3.3"
base_url = "http://my-server:11434/v1/chat/completions"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.provider.as_deref(), Some("ollama"));
    assert_eq!(
        cfg.llm.base_url.as_deref(),
        Some("http://my-server:11434/v1/chat/completions")
    );
}

#[test]
fn parse_ollama_config_no_api_key_required() {
    let toml_str = r#"
[llm]
provider = "ollama"
model = "llama3.3"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    let (provider, key) = cfg
        .resolve_provider()
        .expect("should resolve ollama without key");
    assert_eq!(provider, Provider::Ollama);
    assert!(key.is_empty());
}

#[test]
fn resolve_api_keys_ollama_returns_empty_key() {
    let toml_str = r#"
[llm]
provider = "ollama"
model = "llama3.3"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    let (provider, keys) = cfg.resolve_api_keys().expect("should resolve");
    assert_eq!(provider, Provider::Ollama);
    assert_eq!(keys.len(), 1);
    assert!(keys[0].is_empty());
}

#[test]
fn parse_config_with_base_url_for_cloud_provider() {
    let toml_str = r#"
[llm]
provider = "openai"
model = "gpt-4.1"
base_url = "https://my-azure-proxy.example.com/v1/chat/completions"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.provider.as_deref(), Some("openai"));
    assert_eq!(
        cfg.llm.base_url.as_deref(),
        Some("https://my-azure-proxy.example.com/v1/chat/completions")
    );
}

#[test]
fn parse_realistic_ollama_config() {
    let toml_str = r#"
[user]
name = "Mike"
agent_name = "Buddy"

[llm]
provider = "ollama"
model = "llama3.3"
temperature = 0.7
max_tokens = 4096

[sandbox]
enabled = true
mode = "strict"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.provider.as_deref(), Some("ollama"));
    assert_eq!(cfg.llm.model, "llama3.3");
    assert!(cfg.llm.api_key.is_none());
    assert!(cfg.llm.api_keys.is_empty());
}

#[test]
fn parse_fallback_with_base_url() {
    let toml_str = r#"
[llm]
provider = "ollama"
model = "llama3.3"

[[llm.fallback]]
provider = "openai"
model = "gpt-4.1-mini"
base_url = "https://proxy.example.com/v1/chat/completions"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.llm.fallback.len(), 1);
    assert_eq!(
        cfg.llm.fallback[0].base_url.as_deref(),
        Some("https://proxy.example.com/v1/chat/completions")
    );
}

/// Serializes env-var-mutating channel tests so they don't race each other
/// when cargo test runs them in parallel.
static CHANNEL_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn has_any_native_channel_detects_telegram_env() {
    let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
    // Use a unique env var name to avoid conflicts with real credentials
    std::env::set_var("TELEGRAM_BOT_TOKEN", "test-token-for-unit-test");
    let cfg = Config::default();
    assert!(cfg.has_any_native_channel());
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
}

#[test]
fn has_any_native_channel_false_when_no_creds() {
    let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
    // Temporarily clear all native channel env vars
    let keys = [
        "TELEGRAM_BOT_TOKEN",
        "SLACK_BOT_TOKEN",
        "DISCORD_BOT_TOKEN",
        "TWILIO_ACCOUNT_SID",
        "TEAMS_APP_ID",
        "GOOGLE_CHAT_SERVICE_TOKEN",
    ];
    let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in &keys {
        std::env::remove_var(k);
    }
    let cfg = Config::default();
    // Skip assertion if the OS keychain has real credentials (e.g. dev machine with installed plugins)
    if cfg.detected_native_channels().is_empty() {
        assert!(!cfg.has_any_native_channel());
    }
    // Restore
    for (k, v) in saved {
        if let Some(val) = v {
            std::env::set_var(k, val);
        }
    }
}

#[test]
fn detected_native_channels_returns_configured() {
    // Use a dedicated credential in config to avoid env var races with other tests
    let mut cfg = Config::default();
    cfg.credentials.insert(
        "SLACK_BOT_TOKEN".to_string(),
        CredentialValue::EnvVar("BORG_TEST_DETECTED_NATIVE_SLACK".to_string()),
    );
    std::env::set_var("BORG_TEST_DETECTED_NATIVE_SLACK", "xoxb-test-token");
    let channels = cfg.detected_native_channels();
    std::env::remove_var("BORG_TEST_DETECTED_NATIVE_SLACK");
    assert!(channels.iter().any(|(name, _)| *name == "slack"));
}

#[test]
fn thinking_level_defaults_to_off() {
    let cfg = Config::default();
    assert_eq!(cfg.llm.thinking, ThinkingLevel::Off);
    assert!(cfg.llm.thinking.budget_tokens().is_none());
    assert!(cfg.llm.thinking.openai_reasoning_effort().is_none());
    assert!(!cfg.llm.thinking.is_enabled());
}

#[test]
fn thinking_level_budget_tokens() {
    assert_eq!(ThinkingLevel::Low.budget_tokens(), Some(1024));
    assert_eq!(ThinkingLevel::Medium.budget_tokens(), Some(4096));
    assert_eq!(ThinkingLevel::High.budget_tokens(), Some(16384));
    assert_eq!(ThinkingLevel::Xhigh.budget_tokens(), Some(32768));
}

#[test]
fn thinking_level_openai_reasoning_effort() {
    assert_eq!(ThinkingLevel::Low.openai_reasoning_effort(), Some("low"));
    assert_eq!(
        ThinkingLevel::Medium.openai_reasoning_effort(),
        Some("medium")
    );
    assert_eq!(ThinkingLevel::High.openai_reasoning_effort(), Some("high"));
    assert_eq!(ThinkingLevel::Xhigh.openai_reasoning_effort(), Some("high"));
}

#[test]
fn thinking_level_serde_roundtrip() {
    let level: ThinkingLevel = serde_json::from_str(r#""high""#).unwrap();
    assert_eq!(level, ThinkingLevel::High);
    let json = serde_json::to_string(&level).unwrap();
    assert_eq!(json, r#""high""#);
}

#[test]
fn parse_thinking_level_in_config_toml() {
    let toml_str = r#"
            [llm]
            model = "claude-sonnet-4"
            thinking = "medium"
        "#;
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let mut f = std::fs::File::create(&config_path).unwrap();
    f.write_all(toml_str.as_bytes()).unwrap();

    let cfg = Config::load_from(&config_path).unwrap();
    assert_eq!(cfg.llm.thinking, ThinkingLevel::Medium);
    assert_eq!(cfg.llm.thinking.budget_tokens(), Some(4096));
}

#[test]
fn group_activation_defaults_to_mention() {
    let cfg = Config::default();
    assert_eq!(cfg.gateway.group_activation, ActivationMode::Mention);
}

// -- CollaborationMode --

#[test]
fn collaboration_mode_default_is_default() {
    let cfg = Config::default();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Default
    );
}

#[test]
fn collaboration_mode_from_str_roundtrip() {
    for mode_str in &["default", "execute", "plan"] {
        let mode: CollaborationMode = mode_str.parse().unwrap();
        assert_eq!(&format!("{mode}"), mode_str);
    }
}

#[test]
fn collaboration_mode_from_str_invalid() {
    assert!("bogus".parse::<CollaborationMode>().is_err());
}

#[test]
fn apply_setting_collaboration_mode() {
    let mut cfg = Config::default();
    cfg.apply_setting("conversation.collaboration_mode", "execute")
        .unwrap();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
    cfg.apply_setting("conversation.collaboration_mode", "plan")
        .unwrap();
    assert_eq!(cfg.conversation.collaboration_mode, CollaborationMode::Plan);
    cfg.apply_setting("conversation.collaboration_mode", "default")
        .unwrap();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Default
    );
}

#[test]
fn apply_setting_collaboration_mode_invalid() {
    let mut cfg = Config::default();
    assert!(cfg
        .apply_setting("conversation.collaboration_mode", "bogus")
        .is_err());
}

#[test]
fn collaboration_mode_blocks_mutations_only_for_plan() {
    assert!(!CollaborationMode::Default.blocks_mutations());
    assert!(!CollaborationMode::Execute.blocks_mutations());
    assert!(CollaborationMode::Plan.blocks_mutations());
}

#[test]
fn collaboration_mode_serde_roundtrip() {
    let toml_str = r#"
[conversation]
collaboration_mode = "execute"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
}

// ── ErrorPolicy tests ──

#[test]
fn error_policy_default_is_once() {
    assert_eq!(ErrorPolicy::default(), ErrorPolicy::Once);
}

#[test]
fn error_policy_from_str() {
    assert_eq!(
        "always".parse::<ErrorPolicy>().unwrap(),
        ErrorPolicy::Always
    );
    assert_eq!("once".parse::<ErrorPolicy>().unwrap(), ErrorPolicy::Once);
    assert_eq!(
        "silent".parse::<ErrorPolicy>().unwrap(),
        ErrorPolicy::Silent
    );
    assert_eq!(
        "ALWAYS".parse::<ErrorPolicy>().unwrap(),
        ErrorPolicy::Always
    );
    assert!("invalid".parse::<ErrorPolicy>().is_err());
}

#[test]
fn error_policy_display() {
    assert_eq!(ErrorPolicy::Always.to_string(), "always");
    assert_eq!(ErrorPolicy::Once.to_string(), "once");
    assert_eq!(ErrorPolicy::Silent.to_string(), "silent");
}

#[test]
fn gateway_config_error_policy_defaults() {
    let cfg = GatewayConfig::default();
    assert_eq!(cfg.error_policy, ErrorPolicy::Once);
    assert_eq!(cfg.error_cooldown_ms, 14_400_000);
    assert!(cfg.channel_error_policies.is_empty());
}

#[test]
fn gateway_config_error_policy_serde() {
    let toml_str = r#"
[gateway]
error_policy = "silent"
error_cooldown_ms = 3600000
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.gateway.error_policy, ErrorPolicy::Silent);
    assert_eq!(cfg.gateway.error_cooldown_ms, 3_600_000);
}

#[test]
fn gateway_config_channel_error_policies_serde() {
    let toml_str = r#"
[gateway.channel_error_policies]
telegram = "once"
slack = "always"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(
        cfg.gateway.channel_error_policies.get("telegram"),
        Some(&ErrorPolicy::Once)
    );
    assert_eq!(
        cfg.gateway.channel_error_policies.get("slack"),
        Some(&ErrorPolicy::Always)
    );
}

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

#[test]
fn resolve_credential_or_env_returns_none_for_missing() {
    let config = Config::default();
    assert!(config
        .resolve_credential_or_env("BORG_TEST_NONEXISTENT_CRED_XYZ")
        .is_none());
}

#[test]
fn resolve_credential_or_env_finds_env_var() {
    let config = Config::default();
    let key = "BORG_TEST_RESOLVE_CRED_ENV";
    unsafe { std::env::set_var(key, "env-value") };
    let result = config.resolve_credential_or_env(key);
    unsafe { std::env::remove_var(key) };
    assert_eq!(result.as_deref(), Some("env-value"));
}

#[test]
fn resolve_credential_or_env_skips_empty_env() {
    let config = Config::default();
    let key = "BORG_TEST_RESOLVE_CRED_EMPTY";
    unsafe { std::env::set_var(key, "") };
    let result = config.resolve_credential_or_env(key);
    unsafe { std::env::remove_var(key) };
    assert!(result.is_none());
}

#[test]
fn resolve_credential_or_env_uses_config_over_env() {
    let key = "BORG_TEST_RESOLVE_CRED_PRIORITY";
    unsafe { std::env::set_var(key, "env-value") };

    let mut config = Config::default();
    // A config credential that references the same env var should resolve identically
    config
        .credentials
        .insert(key.to_string(), CredentialValue::EnvVar(key.to_string()));
    let result = config.resolve_credential_or_env(key);
    unsafe { std::env::remove_var(key) };
    assert_eq!(result.as_deref(), Some("env-value"));
}

/// Credential refs with keychain source survive TOML round-trip.
#[test]
fn credential_keychain_ref_toml_roundtrip() {
    let mut config = Config::default();
    config.credentials.insert(
        "TEST_TOKEN".to_string(),
        CredentialValue::Ref(SecretRef::Keychain {
            service: "borg-messaging-test".to_string(),
            account: "borg-TEST_TOKEN".to_string(),
        }),
    );

    let toml_str = toml::to_string_pretty(&config).expect("serialize");
    let parsed: Config = toml::from_str(&toml_str).expect("deserialize");

    assert!(
        parsed.credentials.contains_key("TEST_TOKEN"),
        "credential ref must survive TOML round-trip"
    );
    // Verify it's a Ref, not an EnvVar
    match &parsed.credentials["TEST_TOKEN"] {
        CredentialValue::Ref(SecretRef::Keychain { service, account }) => {
            assert_eq!(service, "borg-messaging-test");
            assert_eq!(account, "borg-TEST_TOKEN");
        }
        other => panic!("expected Keychain ref, got {other:?}"),
    }
}

#[test]
fn resolve_keychain_fallback_returns_none_for_unknown_key() {
    let config = Config::default();
    // Unknown key should not trigger keychain fallback
    assert!(config
        .resolve_keychain_fallback("BORG_TEST_UNKNOWN_KEY_XYZ")
        .is_none());
}

#[test]
fn resolve_keychain_fallback_maps_known_keys() {
    // Verify that known credential keys map to the expected keychain service/account
    // by checking the source code contains the expected mappings
    let source = include_str!("mod.rs");
    assert!(
        source.contains(r#"("TELEGRAM_BOT_TOKEN", "messaging/telegram")"#),
        "KEY_PLUGIN_MAP must include TELEGRAM_BOT_TOKEN"
    );
    assert!(
        source.contains(r#"("SLACK_BOT_TOKEN", "messaging/slack")"#),
        "KEY_PLUGIN_MAP must include SLACK_BOT_TOKEN"
    );
    assert!(
        source.contains(r#"("DISCORD_BOT_TOKEN", "messaging/discord")"#),
        "KEY_PLUGIN_MAP must include DISCORD_BOT_TOKEN"
    );
}

#[test]
fn resolve_credential_or_env_tries_keychain_fallback() {
    // Verify the source code calls resolve_keychain_fallback as a final step
    let source = include_str!("mod.rs");
    assert!(
        source.contains("self.resolve_keychain_fallback(name)"),
        "resolve_credential_or_env must call resolve_keychain_fallback as final fallback"
    );
}

#[test]
fn resolve_credential_or_env_prefers_config_over_keychain() {
    // Config credential should be resolved before keychain fallback is tried
    let key = "BORG_TEST_CONFIG_VS_KC";
    let mut config = Config::default();
    unsafe { std::env::set_var(key, "config-value") };
    config
        .credentials
        .insert(key.to_string(), CredentialValue::EnvVar(key.to_string()));
    let result = config.resolve_credential_or_env(key);
    unsafe { std::env::remove_var(key) };
    assert_eq!(result.as_deref(), Some("config-value"));
}

/// Guard: gateway resolve_credential must delegate to config.resolve_credential_or_env
#[test]
fn gateway_resolve_credential_delegates_to_core() {
    let source = include_str!("../../../gateway/src/channel_init.rs");
    assert!(
        source.contains("config.resolve_credential_or_env(key)"),
        "gateway resolve_credential must delegate to core's resolve_credential_or_env"
    );
}

/// Guard: KEY_PLUGIN_MAP must cover all credential keys used in gateway channel_init.rs.
/// If a new credential is added to channel_init.rs, it must also be added to KEY_PLUGIN_MAP
/// in config/mod.rs so the keychain fallback can resolve it.
#[test]
fn key_plugin_map_covers_all_gateway_credentials() {
    let gateway_source = include_str!("../../../gateway/src/channel_init.rs");
    let config_source = include_str!("mod.rs");

    // Extract all credential keys from resolve_credential() calls in gateway
    for line in gateway_source.lines() {
        let trimmed = line.trim();
        // Match lines like: resolve_credential(config, "messaging/foo", "SOME_KEY");
        if let Some(start) = trimmed.find("resolve_credential(") {
            let after = &trimmed[start..];
            // Extract the third argument (the key)
            let parts: Vec<&str> = after.split('"').collect();
            // Pattern: resolve_credential(config, "plugin_id", "KEY")
            // parts[0] = resolve_credential(config,
            // parts[1] = plugin_id
            // parts[2] = ,
            // parts[3] = KEY
            if parts.len() >= 4 {
                let key = parts[3];
                if key.contains("BORG_TEST") || key.is_empty() {
                    continue; // skip test-only keys
                }
                assert!(
                    config_source.contains(&format!("\"{key}\"")),
                    "KEY_PLUGIN_MAP in config/mod.rs is missing credential key '{key}' \
                     used in gateway/channel_init.rs. Add it to resolve_keychain_fallback()."
                );
            }
        }
    }
}

/// Guard: TUI must reload config after plugin install
#[test]
fn tui_reloads_config_after_plugin_install() {
    let source = include_str!("../../../cli/src/tui/mod.rs");
    // Find the Install block and verify it reloads config
    let install_section: String = source
        .lines()
        .skip_while(|l| !l.contains("PluginAction::Install"))
        .take(80)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        install_section.contains("Config::load_from_db()"),
        "TUI must reload config after plugin install to pick up new credentials"
    );
}
