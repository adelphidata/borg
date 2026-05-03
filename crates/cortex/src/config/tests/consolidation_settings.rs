//! Regression tests for the `consolidation.*` settings registry entries.
//!
//! `consolidation::run_dream` writes `consolidation.last_nightly` /
//! `consolidation.last_weekly` straight to the `settings` table. Before the
//! registry knew about those keys, every config reload after the first
//! Dream run logged `Ignoring invalid setting consolidation.last_nightly:
//! Unknown setting` and the typed `Config` lost the value.

use super::*;
use crate::settings::SETTING_REGISTRY;

#[test]
fn consolidation_last_nightly_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    assert_eq!(cfg.consolidation.last_nightly, None);

    cfg.apply_setting("consolidation.last_nightly", "1714694400")
        .expect("registered key must apply cleanly");
    assert_eq!(cfg.consolidation.last_nightly, Some(1714694400));

    let extract = SETTING_REGISTRY
        .iter()
        .find(|(k, _)| *k == "consolidation.last_nightly")
        .map(|(_, ext)| ext)
        .expect("key must be in SETTING_REGISTRY");
    assert_eq!(extract(&cfg), "1714694400");
}

#[test]
fn consolidation_last_weekly_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    cfg.apply_setting("consolidation.last_weekly", "1715299200")
        .expect("registered key must apply cleanly");
    assert_eq!(cfg.consolidation.last_weekly, Some(1715299200));
}

#[test]
fn empty_string_clears_consolidation_timestamp() {
    let mut cfg = Config::default();
    cfg.consolidation.last_nightly = Some(42);
    cfg.apply_setting("consolidation.last_nightly", "")
        .expect("empty value clears to None");
    assert_eq!(cfg.consolidation.last_nightly, None);
}

#[test]
fn malformed_consolidation_timestamp_errors_via_registered_path() {
    // Pre-fix this would surface as "Unknown setting"; post-fix it is a
    // typed parse error, so the resolver knows the key is real and only
    // the *value* is bad. That distinction is what the dedup-warner uses
    // to keep stale rows out of the log.
    let mut cfg = Config::default();
    let err = cfg
        .apply_setting("consolidation.last_nightly", "not-a-number")
        .expect_err("non-i64 value must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("consolidation.last_nightly") && !msg.contains("Unknown setting"),
        "expected typed parse error, got: {msg}"
    );
}
