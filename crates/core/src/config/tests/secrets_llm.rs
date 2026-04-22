use super::*;
use crate::secrets_resolve::SecretRef;
use std::io::Write;

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
fn resolve_api_keys_overrides_openai_to_openrouter_when_key_is_openrouter() {
    let env = "BORG_TEST_MISMATCHED_KEY";
    std::env::set_var(env, "sk-or-v1-mismatched-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openai".to_string());
    cfg.llm.api_keys = vec![SecretRef::Env {
        var: env.to_string(),
    }];
    let (provider, keys) = cfg.resolve_api_keys().expect("should resolve");
    assert_eq!(provider, Provider::OpenRouter);
    assert_eq!(keys[0], "sk-or-v1-mismatched-key");
    std::env::remove_var(env);
}

#[test]
fn resolve_api_keys_keeps_provider_when_key_prefix_is_ambiguous() {
    let env = "BORG_TEST_AMBIGUOUS_KEY";
    std::env::set_var(env, "sk-some-openai-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openai".to_string());
    cfg.llm.api_keys = vec![SecretRef::Env {
        var: env.to_string(),
    }];
    let (provider, _) = cfg.resolve_api_keys().expect("should resolve");
    assert_eq!(provider, Provider::OpenAi);
    std::env::remove_var(env);
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

// ────────────────────────────────────────────────────────────────────────
// Regression guards for the "onboarded OpenRouter → runtime resolved Ollama"
// incident. Provider must ALWAYS come from explicit config (onboarding,
// /settings, /model, or `borg settings set llm.provider <name>`). Never
// from a TCP probe, an API-key-env-name guess, or any other fuzzy signal.
// ────────────────────────────────────────────────────────────────────────

/// When no provider is configured, resolve_provider MUST bail with a message
/// that tells the user how to configure one. It MUST NOT silently pick a
/// provider (previously: Ollama, if port 11434 was reachable).
#[test]
fn resolve_provider_bails_when_unconfigured_no_silent_fallback() {
    let mut cfg = Config::default();
    cfg.llm.provider = None;
    let err = cfg
        .resolve_provider()
        .expect_err("must not auto-detect a provider when none is configured");
    let msg = format!("{err}");
    assert!(
        msg.contains("No LLM provider configured"),
        "error should explicitly say no provider is configured, got: {msg}"
    );
    assert!(
        msg.contains("borg settings set llm.provider") || msg.contains("borg init"),
        "error should point user at onboarding or settings, got: {msg}"
    );
}

/// The headline regression: even with Ollama happily running on localhost,
/// resolve_provider returns Err — never Ok((Provider::Ollama, _)).
/// This is the exact failure mode the user hit after onboarding OpenRouter.
#[test]
fn resolve_provider_never_silently_returns_ollama() {
    let mut cfg = Config::default();
    cfg.llm.provider = None;
    // Regardless of whether 127.0.0.1:11434 would answer, the result must be Err.
    if let Ok((p, _)) = cfg.resolve_provider() {
        panic!(
            "resolve_provider must error when provider is unset, got Ok({p:?}) — \
             Ollama auto-promotion by TCP probe was the bug"
        );
    }
}

/// When the provider IS configured but the API key env var isn't exported,
/// resolve_provider must bail with a message that names both the provider
/// and the env var. Previously this was masked by the Ollama fallback.
#[test]
fn resolve_provider_bails_loud_when_key_missing_for_configured_provider() {
    // Incident regression: user onboarded OpenRouter but OPENROUTER_API_KEY
    // wasn't in the launch shell → runtime used to silently fall through to
    // Ollama. Must now fail loud pointing at the configured provider.
    let env_name = "BORG_TEST_MISSING_KEY_OPENROUTER_DOES_NOT_EXIST";
    std::env::remove_var(env_name);
    std::env::remove_var("OPENROUTER_API_KEY");

    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    cfg.llm.api_key_env = env_name.to_string();
    cfg.llm.api_key = None;

    let err = cfg
        .resolve_provider()
        .expect_err("expected a loud error, not a silent fallback");
    let msg = format!("{err}");
    assert!(
        msg.contains("openrouter"),
        "error must name the configured provider, got: {msg}"
    );
    assert!(
        msg.contains(env_name) || msg.contains("OPENROUTER_API_KEY"),
        "error must name the env var the user needs to set, got: {msg}"
    );
    assert!(
        !msg.to_lowercase().contains("ollama"),
        "error must not mention Ollama — user did not configure Ollama, got: {msg}"
    );
}

/// resolve_api_keys follows resolve_provider — if provider is None and no
/// api_keys are configured, it must bail, never silently pick Ollama.
#[test]
fn resolve_api_keys_bails_when_provider_unconfigured() {
    let mut cfg = Config::default();
    cfg.llm.provider = None;
    cfg.llm.api_keys.clear();
    cfg.llm.api_key = None;

    let result = cfg.resolve_api_keys();
    assert!(
        result.is_err(),
        "resolve_api_keys must error when no provider is configured, got {result:?}"
    );
}

/// Explicit Ollama stays supported — `provider = "ollama"` is the ONLY way
/// to reach Ollama. Guards against an overcorrection that broke opt-in Ollama.
#[test]
fn resolve_provider_explicit_ollama_still_works() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("ollama".to_string());
    let (provider, key) = cfg
        .resolve_provider()
        .expect("explicit ollama must still resolve");
    assert_eq!(provider, Provider::Ollama);
    assert!(key.is_empty(), "ollama is keyless");
}

/// Explicit Claude CLI is also keyless and stays supported.
#[test]
fn resolve_provider_explicit_claude_cli_still_works() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("claude-cli".to_string());
    let (provider, key) = cfg
        .resolve_provider()
        .expect("explicit claude-cli must still resolve");
    assert_eq!(provider, Provider::ClaudeCli);
    assert!(key.is_empty(), "claude-cli is keyless");
}

/// Every provider string that the onboarding wizard can write MUST round-trip
/// through the DB into a populated `Config.llm.provider`. Catches schema drift
/// between onboarding's DB keys and `SETTING_REGISTRY` — i.e. a future edit
/// where onboarding writes `"llm.provider"` but the loader reads `"provider"`
/// (or vice versa).
#[test]
fn provider_setting_key_roundtrips_through_apply_setting() {
    for name in [
        "openrouter",
        "openai",
        "anthropic",
        "gemini",
        "deepseek",
        "groq",
        "ollama",
        "claude-cli",
    ] {
        let mut cfg = Config::default();
        // This mirrors what Config::load_from_db does for every DB row.
        cfg.apply_setting("provider", name).unwrap_or_else(|e| {
            panic!("apply_setting(\"provider\", \"{name}\") must succeed, got: {e}")
        });
        assert_eq!(
            cfg.llm.provider.as_deref(),
            Some(name),
            "DB key \"provider\" must populate Config.llm.provider for {name}"
        );
    }
}
