use super::*;

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
