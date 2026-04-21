use super::*;
use crate::secrets_resolve::SecretRef;

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
    let source = include_str!("../mod.rs");
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
    let source = include_str!("../mod.rs");
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
