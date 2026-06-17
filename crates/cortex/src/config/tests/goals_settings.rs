//! Regression tests for the `goals.*` settings registry entries.
//!
//! These keys gate the `/goal` LLM tools and the `<goal_context>` system-prompt
//! injection; if the macro arm is silently dropped during a refactor, the gate
//! falls open. The round-trips below catch that.

use super::*;
use crate::settings::SETTING_REGISTRY;

/// Invariant: the `/goal` subsystem defaults ON. Flipping `enabled` to
/// `false` in `GoalsConfig::default` would silently disable the feature
/// for every fresh install — this guard fails loudly if that ever
/// changes by accident.
#[test]
fn goals_defaults_keep_feature_enabled() {
    let cfg = Config::default();
    assert!(cfg.goals.enabled, "feature must default on");
    assert_eq!(cfg.goals.default_token_budget, None);
}

#[test]
fn goals_enabled_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    cfg.apply_setting("goals.enabled", "false")
        .expect("registered key must apply cleanly");
    assert!(!cfg.goals.enabled);

    let extract = SETTING_REGISTRY
        .iter()
        .find(|(k, _)| *k == "goals.enabled")
        .map(|(_, ext)| ext)
        .expect("key must be in SETTING_REGISTRY");
    assert_eq!(extract(&cfg), "false");
}

#[test]
fn goals_default_token_budget_round_trips_and_clears() {
    let mut cfg = Config::default();
    cfg.apply_setting("goals.default_token_budget", "20000")
        .expect("integer parse");
    assert_eq!(cfg.goals.default_token_budget, Some(20000));

    // Empty string clears to None — the unbounded sentinel.
    cfg.apply_setting("goals.default_token_budget", "")
        .expect("empty value clears");
    assert_eq!(cfg.goals.default_token_budget, None);
}

#[test]
fn malformed_goals_bool_errors_via_registered_path() {
    let mut cfg = Config::default();
    let err = cfg
        .apply_setting("goals.enabled", "maybe")
        .expect_err("non-bool must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("goals.enabled") && !msg.contains("Unknown setting"),
        "expected typed parse error, got: {msg}"
    );
}

/// `goals.enabled = false` must remove the three goal tools from the
/// schema sent to the LLM (Tool Conservation invariant — every schema is
/// ~5KB / turn). Catching a regression here is the whole point of the
/// gate; the dispatcher-level guard is defense in depth only.
#[tokio::test]
async fn disabling_goals_drops_tool_schemas() {
    use crate::tool::definitions::core_tool_definitions;

    let mut cfg = Config::default();
    let defs_on = core_tool_definitions(&cfg).await;
    let names_on: Vec<&str> = defs_on.iter().map(|t| t.function.name.as_str()).collect();
    assert!(names_on.contains(&"create_goal"));
    assert!(names_on.contains(&"update_goal"));
    assert!(names_on.contains(&"get_goal"));

    cfg.goals.enabled = false;
    let defs_off = core_tool_definitions(&cfg).await;
    let names_off: Vec<&str> = defs_off.iter().map(|t| t.function.name.as_str()).collect();
    assert!(!names_off.contains(&"create_goal"));
    assert!(!names_off.contains(&"update_goal"));
    assert!(!names_off.contains(&"get_goal"));
}
