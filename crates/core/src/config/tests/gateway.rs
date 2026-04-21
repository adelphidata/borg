use super::*;

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
