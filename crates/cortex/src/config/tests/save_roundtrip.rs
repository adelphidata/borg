use super::*;
use crate::secrets_resolve::SecretRef;

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
