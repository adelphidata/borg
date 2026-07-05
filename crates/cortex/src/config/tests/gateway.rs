use super::*;

// ── Gateway bindings config tests ──

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

#[test]
fn collaboration_mode_default_is_execute() {
    // Borg is autonomous: the default mode is Execute. There is no "Default"
    // variant — see CollaborationMode docs in config/media.rs.
    let cfg = Config::default();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Execute
    );
}

#[test]
fn collaboration_mode_from_str_roundtrip() {
    for mode_str in &["execute", "plan"] {
        let mode: CollaborationMode = mode_str.parse().unwrap();
        assert_eq!(&format!("{mode}"), mode_str);
    }
}

#[test]
fn collaboration_mode_from_str_legacy_default_alias() {
    // Existing DB rows may carry "default" from the pre-removal era; they must
    // resolve to Execute rather than erroring out and breaking the daemon boot.
    let mode: CollaborationMode = "default".parse().unwrap();
    assert_eq!(mode, CollaborationMode::Execute);
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
    // Legacy alias still applies for stored "default" rows.
    cfg.apply_setting("conversation.collaboration_mode", "default")
        .unwrap();
    assert_eq!(
        cfg.conversation.collaboration_mode,
        CollaborationMode::Execute
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
    assert!(!CollaborationMode::Execute.blocks_mutations());
    assert!(CollaborationMode::Plan.blocks_mutations());
}

/// Guard against reintroducing a `Default` collaboration mode variant.
///
/// Borg is autonomous by design — the previous `Default` mode acted like a
/// chat assistant that asked questions instead of executing, which conflicts
/// with the project's positioning. It was deliberately removed; "default"
/// survives only as a `FromStr` alias for legacy DB rows. Any attempt to
/// re-add a `Default` variant must be challenged.
///
/// This is a source-grep guard (allowed by CLAUDE.md as an "invariant types
/// can't express" exception, like the prompt-cache and TUI mouse guards).
#[test]
fn collaboration_mode_default_variant_must_not_return() {
    let src = include_str!("../media.rs");
    // Either form of the variant path would only ever appear if the variant
    // were re-added — `#[default]` (the attr) and `Default for ...` (the trait
    // impl) use different casing/punctuation and won't trip this check.
    assert!(
        !src.contains("Self::Default") && !src.contains("CollaborationMode::Default"),
        "CollaborationMode::Default has been removed; do not reintroduce it. \
         Borg is autonomous — Execute is the default mode. See the type doc \
         in config/media.rs.",
    );
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

// `ErrorPolicy` FromStr/Display round-trips (incl. case-insensitivity and
// the unknown-value error) are covered by the unit tests colocated with the
// type in `config/gateway.rs`; not duplicated here.

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
