use super::*;
#[allow(unused_imports)]
use crate::security::secrets_resolve::SecretRef;
use std::io::Write;

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
fn load_from_nonexistent_path_returns_usable_config() {
    // Missing config file is treated as empty, not as an error — and the
    // returned Config must remain mutable via apply_setting so the daemon
    // can boot with no config.toml on disk.
    let path = Path::new("/tmp/borg_test_nonexistent_config_path.toml");
    assert!(!path.exists(), "test precondition: file must not exist");
    let mut cfg = Config::load_from(path).expect("missing file should not error");
    cfg.apply_setting("model", "post-load-model")
        .expect("config from missing file must still accept apply_setting");
    assert_eq!(cfg.llm.model, "post-load-model");
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

/// Table-driven check that `define_settings!` dispatches each key to the right
/// field. Every row: apply (key, value) to a default config, then read the
/// field via `getter` and confirm the stringified result round-trips. Catches
/// missing arms, mis-typed keys, and dispatch-to-wrong-field regressions
/// without one near-duplicate test per setting.
///
/// Limitation: same-typed field swaps (e.g. `tts.default_voice` ↔ `tts.default_format`,
/// both `String`) would slip through because each row only checks its own getter.
/// Validator wiring is pinned separately by `apply_setting_*_rejects_*` /
/// `apply_setting_*_invalid` tests below.
#[test]
fn apply_setting_dispatches_to_correct_field() {
    type Getter = fn(&Config) -> String;
    let cases: &[(&str, &str, Getter)] = &[
        ("model", "gpt-4o", |c| c.llm.model.clone()),
        ("temperature", "0.5", |c| c.llm.temperature.to_string()),
        ("conversation.max_iterations", "50", |c| {
            c.conversation.max_iterations.to_string()
        }),
        ("conversation.show_thinking", "false", |c| {
            c.conversation.show_thinking.to_string()
        }),
        ("security.secret_detection", "false", |c| {
            c.security.secret_detection.to_string()
        }),
        ("browser.headless", "false", |c| {
            c.browser.headless.to_string()
        }),
        ("tts.enabled", "true", |c| c.tts.enabled.to_string()),
        ("tts.auto_mode", "true", |c| c.tts.auto_mode.to_string()),
        ("tts.default_voice", "nova", |c| c.tts.default_voice.clone()),
        ("tts.default_format", "opus", |c| {
            c.tts.default_format.clone()
        }),
        ("budget.monthly_token_limit", "1000000", |c| {
            c.budget.monthly_token_limit.to_string()
        }),
        ("budget.warning_threshold", "0.9", |c| {
            c.budget.warning_threshold.to_string()
        }),
    ];
    for (key, value, getter) in cases {
        let mut cfg = Config::default();
        cfg.apply_setting(key, value)
            .unwrap_or_else(|e| panic!("apply_setting({key} = {value}) failed: {e}"));
        let actual = getter(&cfg);
        assert_eq!(
            &actual, value,
            "after apply_setting({key} = {value}), field reads back as {actual:?}"
        );
    }
}

#[test]
fn apply_setting_temperature_out_of_range() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("temperature", "3.0").is_err());
    assert!(cfg.apply_setting("temperature", "-1.0").is_err());
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
fn apply_setting_memory_active_injection_round_trip() {
    // Guards the registry+apply+extractor wiring for the three new keys —
    // a missing arm in `define_settings!` would silently drop the setter.
    let mut cfg = Config::default();
    assert!(cfg.memory.active_injection.enabled);
    cfg.apply_setting("memory.active_injection.enabled", "false")
        .expect("toggle off");
    assert!(!cfg.memory.active_injection.enabled);
    cfg.apply_setting("memory.active_injection.top_k", "12")
        .expect("set top_k");
    assert_eq!(cfg.memory.active_injection.top_k, 12);
    cfg.apply_setting("memory.active_injection.max_tokens", "800")
        .expect("set max_tokens");
    assert_eq!(cfg.memory.active_injection.max_tokens, 800);
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

/// Table-driven check that partial TOML for one section leaves unset fields
/// at their `#[serde(default)]` values rather than failing to parse. A
/// regression where someone forgets `#[serde(default)]` on a new field shows
/// up here, and we don't need one near-duplicate test per section.
#[test]
fn partial_toml_keeps_defaults_for_unset_fields() {
    type Assert = fn(&Config, &Config);
    let defaults = Config::default();
    let cases: &[(&str, &str, Assert)] = &[
        (
            "llm-with-only-model",
            "[llm]\nmodel = \"custom-model\"\n",
            |cfg, d| {
                assert_eq!(cfg.llm.model, "custom-model");
                assert_eq!(cfg.llm.temperature, d.llm.temperature);
                assert_eq!(cfg.llm.max_tokens, d.llm.max_tokens);
                assert_eq!(cfg.llm.max_retries, d.llm.max_retries);
                assert_eq!(cfg.llm.api_key_env, d.llm.api_key_env);
                assert!(cfg.llm.base_url.is_none());
            },
        ),
        (
            "web-with-only-enabled",
            "[web]\nenabled = false\n",
            |cfg, d| {
                assert!(!cfg.web.enabled);
                assert_eq!(cfg.web.search_provider, d.web.search_provider);
                assert_eq!(cfg.web.search_api_key_env, d.web.search_api_key_env);
            },
        ),
        (
            "tasks-with-only-max-concurrent",
            "[tasks]\nmax_concurrent = 5\n",
            |cfg, _d| {
                assert_eq!(cfg.tasks.max_concurrent, 5);
            },
        ),
        ("empty-toml-budget-defaults", "", |cfg, d| {
            assert_eq!(cfg.budget.monthly_token_limit, d.budget.monthly_token_limit);
            assert_eq!(cfg.budget.warning_threshold, d.budget.warning_threshold);
        }),
        (
            "llm-with-only-model-yields-default-api-key",
            "[llm]\nmodel = \"meta/llama-3\"\n",
            |cfg, d| {
                assert_eq!(cfg.llm.model, "meta/llama-3");
                assert_eq!(cfg.llm.api_key_env, d.llm.api_key_env);
            },
        ),
    ];
    for (name, toml_str, assert) in cases {
        let cfg: Config = toml::from_str(toml_str).unwrap_or_else(|e| panic!("parse {name}: {e}"));
        assert(&cfg, &defaults);
    }
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
fn telemetry_config_parsing() {
    let toml_str = r#"
[telemetry]
tracing_enabled = true
metrics_enabled = true
cost_events = true
otlp_endpoint = "http://otel:4317"
service_name = "my-borg"
sampling_ratio = 0.5
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.telemetry.tracing_enabled);
    assert!(cfg.telemetry.metrics_enabled);
    assert!(cfg.telemetry.cost_events);
    assert_eq!(cfg.telemetry.otlp_endpoint, "http://otel:4317");
    assert_eq!(cfg.telemetry.service_name, "my-borg");
    assert!((cfg.telemetry.sampling_ratio - 0.5).abs() < f64::EPSILON);
}

#[test]
fn apply_setting_telemetry_tracing_is_user_settable() {
    // Telemetry became user-configurable when the OpenTelemetry pipeline
    // shipped (registered in the settings table + /settings popup), so
    // apply_setting must accept the key and actually mutate the field.
    let mut cfg = Config::default();
    cfg.telemetry.tracing_enabled = false;
    cfg.apply_setting("telemetry.tracing_enabled", "true")
        .expect("telemetry.tracing_enabled is a registered bool setting");
    assert!(cfg.telemetry.tracing_enabled);
}

/// A bad value says what to type instead, not what went wrong in the parser.
#[test]
fn a_bad_setting_value_names_what_it_takes() {
    let err = parse_value::<u64>("abc", "agents.delegate_timeout_secs").unwrap_err();
    assert_eq!(
        err.to_string(),
        "agents.delegate_timeout_secs takes a whole number of 0 or more, not \"abc\"."
    );
    let err = parse_value::<bool>("maybe", "web.enabled").unwrap_err();
    assert_eq!(err.to_string(), "web.enabled takes true or false, not \"maybe\".");
    let err = parse_value::<f64>("x", "llm.temperature").unwrap_err();
    assert_eq!(err.to_string(), "llm.temperature takes a number, not \"x\".");
}
