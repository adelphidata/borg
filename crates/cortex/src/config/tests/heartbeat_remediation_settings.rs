//! Regression tests for the `heartbeat.remediation_*` settings registry
//! entries.
//!
//! These keys gate whether a scheduled heartbeat that detects something
//! actionable escalates from the cheap tool-less check-in to the full agent
//! loop (tools + the goal/Ralph loop) and how that remediation is bounded. A
//! dropped macro arm would silently strand the feature at its defaults with no
//! way to tune or disable it; the round-trips below catch that.

use super::*;
use crate::settings::SETTING_REGISTRY;

/// Defaults: remediation is ON, with a generous-but-bounded turn budget, a
/// per-goal token cap, and a max-ticks backstop. Changing any of these in
/// `HeartbeatConfig::default` shifts behavior for every fresh install, so pin
/// them.
#[test]
fn remediation_defaults_are_enabled_and_bounded() {
    let cfg = Config::default();
    assert!(cfg.heartbeat.remediation_enabled, "feature defaults on");
    assert_eq!(cfg.heartbeat.remediation_timeout_secs, 600);
    assert_eq!(cfg.heartbeat.remediation_token_budget, 50_000);
    assert_eq!(cfg.heartbeat.remediation_max_ticks, 12);
}

#[test]
fn remediation_enabled_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    cfg.apply_setting("heartbeat.remediation_enabled", "false")
        .expect("registered key must apply cleanly");
    assert!(!cfg.heartbeat.remediation_enabled);

    let extract = SETTING_REGISTRY
        .iter()
        .find(|(k, _)| *k == "heartbeat.remediation_enabled")
        .map(|(_, ext)| ext)
        .expect("key must be in SETTING_REGISTRY");
    assert_eq!(extract(&cfg), "false");
}

#[test]
fn remediation_numeric_knobs_round_trip() {
    let mut cfg = Config::default();
    cfg.apply_setting("heartbeat.remediation_timeout_secs", "900")
        .expect("u64 parse");
    cfg.apply_setting("heartbeat.remediation_token_budget", "12345")
        .expect("u64 parse");
    cfg.apply_setting("heartbeat.remediation_max_ticks", "5")
        .expect("u32 parse");
    assert_eq!(cfg.heartbeat.remediation_timeout_secs, 900);
    assert_eq!(cfg.heartbeat.remediation_token_budget, 12345);
    assert_eq!(cfg.heartbeat.remediation_max_ticks, 5);
}

#[test]
fn malformed_remediation_max_ticks_errors_via_registered_path() {
    let mut cfg = Config::default();
    let err = cfg
        .apply_setting("heartbeat.remediation_max_ticks", "lots")
        .expect_err("non-integer must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("heartbeat.remediation_max_ticks") && !msg.contains("Unknown setting"),
        "expected typed parse error, got: {msg}"
    );
}
