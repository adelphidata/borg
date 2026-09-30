use super::*;
use crate::security::secrets_resolve::SecretRef;
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
fn parse_config_with_secret_ref_keychain() {
    let toml_str = r#"
[llm]
provider = "openrouter"
api_key = { source = "keychain", service = "borg-openrouter", account = "borg" }
model = "anthropic/claude-sonnet-4"
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(cfg.llm.api_key.is_some());
    if let Some(SecretRef::Keychain { service, account }) = &cfg.llm.api_key {
        assert_eq!(service, "borg-openrouter");
        assert_eq!(account, "borg");
    } else {
        panic!("expected Keychain variant");
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
fn resolve_provider_bedrock_uses_region_without_api_key() {
    let _lock = crate::llm::bedrock::env_test_lock();
    std::env::set_var("AWS_REGION", "us-west-2");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("bedrock".to_string());
    let (provider, key) = cfg.resolve_provider().expect("bedrock should resolve");
    assert_eq!(provider, Provider::Bedrock);
    assert!(key.is_empty(), "bedrock must not require an API key");
    std::env::remove_var("AWS_REGION");
}

#[test]
fn resolve_provider_vertex_uses_cloud_auth_without_api_key() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("vertex-ai".to_string());
    let (provider, key) = cfg.resolve_provider().expect("vertex should resolve");
    assert_eq!(provider, Provider::VertexAi);
    assert!(key.is_empty(), "vertex uses GCP auth, not static API keys");
}

#[test]
fn resolve_provider_azure_openai_allows_entra_without_api_key() {
    let env_name = "BORG_TEST_EMPTY_AZURE_KEY";
    let old_azure_key = std::env::var("AZURE_OPENAI_API_KEY").ok();
    std::env::remove_var(env_name);
    std::env::remove_var("AZURE_OPENAI_API_KEY");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("azure-openai".to_string());
    cfg.llm.api_key_env = env_name.to_string();
    let (provider, key) = cfg
        .resolve_provider()
        .expect("azure-openai should allow Entra auth without API key");
    assert_eq!(provider, Provider::AzureOpenAi);
    assert!(key.is_empty(), "empty key selects Entra token path");
    if let Some(old) = old_azure_key {
        std::env::set_var("AZURE_OPENAI_API_KEY", old);
    }
}

#[test]
fn resolve_provider_bedrock_errors_when_region_missing() {
    let _lock = crate::llm::bedrock::env_test_lock();
    // Point HOME at an empty tempdir so the ~/.aws/config profile fallback
    // can't find a region either — otherwise the developer's real AWS
    // config shadows the failure case under test.
    let temp = tempfile::tempdir().expect("tempdir");
    let old_home = std::env::var("HOME").ok();
    let old_profile = std::env::var("AWS_PROFILE").ok();
    let old_default_profile = std::env::var("AWS_DEFAULT_PROFILE").ok();
    std::env::set_var("HOME", temp.path());
    std::env::remove_var("AWS_PROFILE");
    std::env::remove_var("AWS_DEFAULT_PROFILE");
    std::env::remove_var("AWS_REGION");
    std::env::remove_var("AWS_DEFAULT_REGION");

    let mut cfg = Config::default();
    cfg.llm.provider = Some("bedrock".to_string());
    let err = cfg
        .resolve_provider()
        .expect_err("missing region must error");
    assert!(
        err.to_string().contains("AWS region"),
        "unexpected error: {err}"
    );

    if let Some(h) = old_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(p) = old_profile {
        std::env::set_var("AWS_PROFILE", p);
    }
    if let Some(p) = old_default_profile {
        std::env::set_var("AWS_DEFAULT_PROFILE", p);
    }
}

#[test]
fn resolve_bedrock_region_prefers_setting_over_env() {
    let _lock = crate::llm::bedrock::env_test_lock();
    std::env::set_var("AWS_REGION", "us-east-1");
    let mut cfg = Config::default();
    cfg.llm.bedrock_region = Some("eu-west-1".to_string());
    let region = cfg.resolve_bedrock_region().expect("region resolves");
    assert_eq!(region, "eu-west-1");
    std::env::remove_var("AWS_REGION");
}

#[test]
fn resolve_bedrock_region_reads_shared_aws_profile_config() {
    let _lock = crate::llm::bedrock::env_test_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join(".aws")).expect("create .aws");
    std::fs::write(
        temp.path().join(".aws/config"),
        "[profile bedrock]\nregion = ap-south-1\n",
    )
    .expect("write config");

    let old_home = std::env::var("HOME").ok();
    let old_profile = std::env::var("AWS_PROFILE").ok();
    let old_region = std::env::var("AWS_REGION").ok();
    let old_default_region = std::env::var("AWS_DEFAULT_REGION").ok();

    std::env::set_var("HOME", temp.path());
    std::env::set_var("AWS_PROFILE", "bedrock");
    std::env::remove_var("AWS_REGION");
    std::env::remove_var("AWS_DEFAULT_REGION");

    let cfg = Config::default();
    let region = cfg
        .resolve_bedrock_region()
        .expect("region should come from ~/.aws/config");
    assert_eq!(region, "ap-south-1");

    if let Some(home) = old_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(profile) = old_profile {
        std::env::set_var("AWS_PROFILE", profile);
    } else {
        std::env::remove_var("AWS_PROFILE");
    }
    if let Some(region) = old_region {
        std::env::set_var("AWS_REGION", region);
    } else {
        std::env::remove_var("AWS_REGION");
    }
    if let Some(region) = old_default_region {
        std::env::set_var("AWS_DEFAULT_REGION", region);
    } else {
        std::env::remove_var("AWS_DEFAULT_REGION");
    }
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
fn resolve_api_keys_keyless_provider_ignores_stray_api_keys() {
    // Strict provider resolution: an operator running Ollama with a leftover
    // OpenRouter key in `llm.api_keys` must stay on Ollama —
    // `correct_provider_from_key` silently flipping the provider would route
    // local traffic to a public cloud.
    let env = "BORG_TEST_STRAY_KEY_FOR_OLLAMA";
    std::env::set_var(env, "sk-or-v1-leftover-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("ollama".to_string());
    cfg.llm.api_keys = vec![SecretRef::Env {
        var: env.to_string(),
    }];
    let (provider, keys) = cfg.resolve_api_keys().expect("should resolve");
    assert_eq!(provider, Provider::Ollama);
    assert_eq!(keys, vec![String::new()]);
    std::env::remove_var(env);
}

#[test]
fn resolve_provider_custom_keyless_with_base_url() {
    // The documented llama.cpp / LM Studio / vLLM flow: `llm.provider =
    // "custom"` + `llm.base_url`, no API key. Must resolve with an empty key
    // rather than bailing "No API key found".
    let mut cfg = Config::default();
    cfg.llm.provider = Some("custom".to_string());
    cfg.llm.base_url = Some("http://localhost:8080/v1/chat/completions".to_string());
    let (provider, key) = cfg.resolve_provider().expect("should resolve keyless");
    assert_eq!(provider, Provider::Custom);
    assert_eq!(key, "");
}

#[test]
fn custom_provider_never_flipped_by_key_prefix() {
    // A recognizable key prefix used as a custom endpoint's bearer credential
    // must not reroute traffic away from the operator's explicit endpoint.
    assert_eq!(
        correct_provider_from_key(Provider::Custom, "sk-or-v1-endpoint-cred"),
        Provider::Custom
    );
    // Sanity: the correction still applies to non-custom providers.
    assert_eq!(
        correct_provider_from_key(Provider::OpenAi, "sk-or-v1-endpoint-cred"),
        Provider::OpenRouter
    );
}

/// Serializes tests that mutate the process-global `OPENROUTER_API_KEY` env
/// var (the built-in `api_key_env` default). Without this they race in the
/// parallel test runner and observe each other's set/remove.
fn openrouter_env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::constants::default_llm_api_key_env_lock()
}

#[test]
fn chosen_provider_key_beats_the_default_openrouter_env() {
    // Regression: with both keys on the machine, switching the provider away
    // from OpenRouter must send that provider's own key. The untouched
    // `api_key_env` default used to win, and `correct_provider_from_key` then
    // routed the turn back to OpenRouter.
    let _env = openrouter_env_lock();
    let old = std::env::var("OPENROUTER_API_KEY").ok();
    let old_ds = std::env::var("DEEPSEEK_API_KEY").ok();
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-v1-host-key");
    std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek-own-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("deepseek".to_string());
    let own = cfg.resolve_api_keys();

    // The default is still the last resort when the provider's own variable
    // is absent — an OpenRouter key left under another provider keeps working.
    std::env::remove_var("DEEPSEEK_API_KEY");
    let fallback = cfg.resolve_api_keys();

    // And an env var the operator named outright still outranks the provider's.
    std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek-own-key");
    std::env::set_var("BORG_TEST_EXPLICIT_LLM_KEY", "sk-explicit");
    cfg.llm.api_key_env = "BORG_TEST_EXPLICIT_LLM_KEY".to_string();
    let explicit = cfg.resolve_provider();

    std::env::remove_var("BORG_TEST_EXPLICIT_LLM_KEY");
    match old {
        Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
        None => std::env::remove_var("OPENROUTER_API_KEY"),
    }
    match old_ds {
        Some(v) => std::env::set_var("DEEPSEEK_API_KEY", v),
        None => std::env::remove_var("DEEPSEEK_API_KEY"),
    }

    let (provider, keys) = own.expect("own key resolves");
    assert_eq!(provider, Provider::DeepSeek);
    assert_eq!(keys, vec!["sk-deepseek-own-key".to_string()]);
    let (provider, keys) = fallback.expect("default env still a fallback");
    assert_eq!(provider, Provider::OpenRouter);
    assert_eq!(keys, vec!["sk-or-v1-host-key".to_string()]);
    assert_eq!(explicit.expect("explicit env").1, "sk-explicit");
}

#[test]
fn resolve_provider_custom_ignores_default_openrouter_env() {
    // Regression: a host with OPENROUTER_API_KEY exported (the built-in
    // `api_key_env` default) must not have that key resolved for a custom
    // endpoint — it would be sent as a Bearer credential to a user-supplied
    // URL and flip provider routing.
    let _env = openrouter_env_lock();
    let old = std::env::var("OPENROUTER_API_KEY").ok();
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-v1-host-key");
    let mut cfg = Config::default();
    cfg.llm.provider = Some("custom".to_string());
    cfg.llm.base_url = Some("http://localhost:8080/v1/chat/completions".to_string());
    let (provider, key) = cfg.resolve_provider().expect("should resolve");
    match old {
        Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
        None => std::env::remove_var("OPENROUTER_API_KEY"),
    }
    assert_eq!(provider, Provider::Custom);
    assert_eq!(
        key, "",
        "default env key must not attach to a custom endpoint"
    );
}

#[test]
fn resolve_provider_custom_without_base_url_errors_actionably() {
    let mut cfg = Config::default();
    cfg.llm.provider = Some("custom".to_string());
    let err = cfg.resolve_provider().expect_err("must not resolve");
    assert!(
        err.to_string().contains("llm.base_url"),
        "error must point at the missing base_url, got: {err}"
    );
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

// ── Provider failover config tests ──

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
    assert!(cfg.llm.thinking.anthropic_budget_tokens().is_none());
    assert!(cfg.llm.thinking.openai_reasoning_effort().is_none());
    assert!(!cfg.llm.thinking.is_enabled());
    assert!(
        cfg.llm.thinking_overrides.is_empty(),
        "no per-model overrides by default"
    );
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
    assert_eq!(cfg.llm.thinking.anthropic_budget_tokens(), Some(4096));
}

#[test]
fn group_activation_defaults_to_mention() {
    let cfg = Config::default();
    assert_eq!(cfg.gateway.group_activation, ActivationMode::Mention);
}

// Regression guards for the "onboarded OpenRouter → runtime resolved Ollama"
// incident. Provider must ALWAYS come from explicit config (onboarding,
// /settings, /model, or `borg settings set llm.provider <name>`). Never
// from a TCP probe, an API-key-env-name guess, or any other fuzzy signal.

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
        msg.contains("No LLM provider selected"),
        "error should explicitly say no provider is selected, got: {msg}"
    );
    assert!(
        msg.contains("/settings") || msg.contains("/model"),
        "error should point user at the TUI popups where they pick a provider, got: {msg}"
    );
}

/// When the provider IS configured but the API key env var isn't exported,
/// resolve_provider must bail with a message that names both the provider
/// and the env var. Previously this was masked by the Ollama fallback.
#[test]
fn resolve_provider_bails_loud_when_key_missing_for_configured_provider() {
    // Incident regression: user onboarded OpenRouter but OPENROUTER_API_KEY
    // wasn't in the launch shell → runtime used to silently fall through to
    // Ollama. Must now fail loud pointing at the configured provider.
    let _env = openrouter_env_lock();
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

/// Second bail path in resolve_api_keys: `api_keys` populated but `provider`
/// is None. Previously defaulted silently to OpenRouter — that's the same
/// "guess a provider the user didn't pick" antipattern the incident fixed.
#[test]
fn resolve_api_keys_bails_when_keys_set_but_provider_unconfigured() {
    let env_name = "BORG_TEST_API_KEYS_NO_PROVIDER_BAIL";
    std::env::set_var(env_name, "sk-or-v1-test");

    let mut cfg = Config::default();
    cfg.llm.provider = None;
    cfg.llm.api_keys = vec![SecretRef::Env {
        var: env_name.to_string(),
    }];

    let err = cfg
        .resolve_api_keys()
        .expect_err("must not infer provider from a key when none is configured");
    let msg = format!("{err}");

    std::env::remove_var(env_name);

    assert!(
        msg.to_lowercase().contains("provider"),
        "error should mention the missing provider, got: {msg}"
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
        "xai",
        "azure-openai",
        "vertex-ai",
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

/// The GUI onboarding stores a key only as a `credentials.{provider}` row.
/// If the loader stops folding that row into `llm.api_key`, a fresh desktop
/// install finishes setup with no key to send — each row below is one way
/// that fold can go wrong.
#[test]
fn stored_api_key_row_feeds_the_llm_resolver() {
    let literal = r#"{"type":"literal","value":"sk-stored"}"#.to_string();
    let keychain = r#"{"type":"keychain","service":"borg-openai","account":"borg"}"#.to_string();
    type Case = (
        &'static str,
        Option<&'static str>,
        Option<SecretRef>,
        Vec<(String, String)>,
        Option<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "literal row for the configured provider resolves",
            Some("openai"),
            None,
            vec![("openai".into(), literal.clone())],
            Some("sk-stored"),
        ),
        (
            "provider match is case-insensitive",
            Some("OpenAI"),
            None,
            vec![("openai".into(), literal.clone())],
            Some("sk-stored"),
        ),
        (
            "another provider's row is not borrowed",
            Some("anthropic"),
            None,
            vec![("openai".into(), literal.clone())],
            None,
        ),
        (
            "explicit llm.api_key wins over the stored row",
            Some("openai"),
            Some(SecretRef::Literal {
                value: "sk-explicit".into(),
            }),
            vec![("openai".into(), literal.clone())],
            Some("sk-explicit"),
        ),
        (
            "no provider means nothing to fold",
            None,
            None,
            vec![("openai".into(), literal.clone())],
            None,
        ),
        (
            "malformed JSON is skipped, not adopted",
            Some("openai"),
            None,
            vec![("openai".into(), "{not json".into())],
            None,
        ),
    ];
    for (name, provider, explicit, rows, want) in cases {
        let mut cfg = Config::default();
        cfg.llm.provider = provider.map(str::to_string);
        cfg.llm.api_key = explicit;
        cfg.adopt_stored_api_key(&rows);
        let got = cfg
            .llm
            .api_key
            .as_ref()
            .map(|r| r.resolve().expect("resolves"));
        assert_eq!(got.as_deref(), want, "{name}");
    }

    // A keychain row becomes keychain routing, not a literal.
    let mut cfg = Config::default();
    cfg.llm.provider = Some("openai".into());
    cfg.adopt_stored_api_key(&[("openai".into(), keychain)]);
    match cfg.llm.api_key {
        Some(SecretRef::Keychain { service, account }) => {
            assert_eq!(
                (service.as_str(), account.as_str()),
                ("borg-openai", "borg")
            );
        }
        other => panic!("expected keychain routing, got {other:?}"),
    }
}

/// An example file that is copied unchanged puts a placeholder where the key
/// goes. It must give the same guidance as a missing key, and it must not be
/// returned as the key.
#[test]
fn resolve_provider_treats_an_example_placeholder_as_no_key() {
    let _env = openrouter_env_lock();
    let saved = std::env::var("OPENROUTER_API_KEY").ok();
    let custom_var = "BORG_TEST_PLACEHOLDER_KEY_ENV";

    let mut cfg = Config::default();
    cfg.llm.provider = Some("openrouter".to_string());
    cfg.llm.api_key = None;

    // Through the provider default variable.
    std::env::set_var("OPENROUTER_API_KEY", "your-api-key-here");
    let default_var = cfg.resolve_provider();

    // Through an explicit `llm.api_key_env`.
    std::env::remove_var("OPENROUTER_API_KEY");
    std::env::set_var(custom_var, "sk-xxxx");
    cfg.llm.api_key_env = custom_var.to_string();
    let explicit_var = cfg.resolve_provider();

    // A real key in the same variable still resolves.
    std::env::set_var(custom_var, "sk-or-v1-9f3a5c7e1b2d4f6a");
    let real = cfg.resolve_provider();

    std::env::remove_var(custom_var);
    match saved {
        Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
        None => std::env::remove_var("OPENROUTER_API_KEY"),
    }

    for (name, result) in [("default var", default_var), ("explicit var", explicit_var)] {
        let msg = result.expect_err("a placeholder is not a key").to_string();
        assert!(
            msg.contains("No API key found for openrouter"),
            "{name}: {msg}"
        );
    }
    let (_, key) = real.expect("a real key resolves");
    assert_eq!(key, "sk-or-v1-9f3a5c7e1b2d4f6a");
}

/// A provider with no model must not send an empty model id upstream:
/// OpenRouter answers that with `400 No models provided`.
#[test]
fn provider_without_model_adopts_the_catalog_default() {
    let rows = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>()
    };
    let recommended = crate::llm::model_catalog::default_model("openrouter")
        .expect("openrouter declares a recommended model");

    let cfg = Config::from_setting_rows(rows(&[("provider", "openrouter")]));
    assert_eq!(cfg.llm.model, recommended);

    // An explicit model always wins over the recommendation.
    let cfg = Config::from_setting_rows(rows(&[("provider", "openrouter"), ("model", "x/y")]));
    assert_eq!(cfg.llm.model, "x/y");

    // No provider yet: nothing to recommend, onboarding still owns the pick.
    let cfg = Config::from_setting_rows(rows(&[]));
    assert_eq!(cfg.llm.model, "");
}
