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
fn omitting_policy_section_keeps_subagent_deny_locked_down() {
    // A `[tools]` section with no nested `[tools.policy]` must still apply
    // the security-relevant defaults via `#[serde(default)]`. The invariant
    // that matters: subagents are denied `schedule`/`browser` out of the box,
    // so a forgotten policy section can't silently let a child agent schedule
    // jobs or drive a browser. Pin the concrete values, not default-vs-default.
    let toml_str = r#"
[tools]
default_timeout_ms = 30000
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert_eq!(cfg.tools.policy.profile, "full");
    assert!(cfg.tools.policy.allow.is_empty());
    assert!(cfg.tools.policy.deny.is_empty());
    assert!(cfg
        .tools
        .policy
        .subagent_deny
        .contains(&"schedule".to_string()));
    assert!(cfg
        .tools
        .policy
        .subagent_deny
        .contains(&"browser".to_string()));
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
