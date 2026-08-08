//! Global emergency stop (`borg pause` / `borg resume`).
//!
//! Borg has no per-tool approval prompts, so the operator's stop lever is a
//! daemon-wide pause that halts NEW work while never killing in-flight work.
//! The flag lives in the settings DB (`security.emergency_paused`), so a CLI
//! write from another process takes effect on the running daemon without a
//! restart. Every gate reads the DB directly through [`is_engaged`] rather
//! than a cached `Config` — the config watcher's loader deliberately falls
//! back to defaults on an unparseable value, which would invert the fail-safe
//! this module guarantees.
//!
//! Fail-safe contract:
//! * missing row / `"false"` → not paused (the default);
//! * `"true"` → paused;
//! * any other stored value, or a DB read error → **paused** (a stop lever
//!   that can be disabled by corrupting its own flag is not a stop lever).
//!
//! Gated seams (all in `borgd`): scheduled-task dispatch, session-executor
//! turn start, and session-heartbeat prompt injection. Internal maintenance
//! (self-heal sweep, log pruning, stalled-task recovery) is *not* agent work
//! and keeps running while paused.

use crate::db::Database;

/// Settings-table key for the pause flag. Registered in `define_settings!`
/// so `borg settings set/get` and the TUI popup see it like any other key.
pub const SETTING_KEY: &str = "security.emergency_paused";

/// Base of the notice returned to interactive/gateway turns while paused.
pub const PAUSED_NOTICE: &str = "borg is paused (borg resume to continue)";

/// Interpret the raw stored setting value per the fail-safe contract.
///
/// `None` (row never written) is the default: not paused. A value that
/// parses as a bool is taken at face value. Anything else is corrupt and
/// fails safe to paused — deliberately including non-canonical spellings
/// like `"TRUE"`/`"1"`, which the validated settings write path never
/// produces.
pub fn interpret_flag(raw: Option<&str>) -> bool {
    match raw {
        None => false,
        Some(v) => match v.trim().parse::<bool>() {
            Ok(b) => b,
            Err(_) => {
                tracing::warn!(
                    value = %v,
                    "security.emergency_paused holds a non-boolean value — failing safe to PAUSED"
                );
                true
            }
        },
    }
}

/// Cheap DB-backed gate read. A DB read error fails safe to paused.
pub async fn is_engaged(db: &Database) -> bool {
    match db.get_setting(SETTING_KEY).await {
        Ok(raw) => interpret_flag(raw.as_deref()),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "emergency-pause flag unreadable — failing safe to PAUSED"
            );
            true
        }
    }
}

/// Operator-facing pause context: `(reason, since_unix)`. Best-effort —
/// meta read errors degrade to `None` with a warn, never block the caller.
pub async fn details(db: &Database) -> (Option<String>, Option<i64>) {
    let reason = match db
        .get_meta(crate::constants::META_EMERGENCY_PAUSE_REASON)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "emergency-pause reason unreadable");
            None
        }
    };
    let since = match db
        .get_meta(crate::constants::META_EMERGENCY_PAUSE_SINCE)
        .await
    {
        Ok(s) => s.and_then(|v| v.parse::<i64>().ok()),
        Err(e) => {
            tracing::warn!(error = %e, "emergency-pause timestamp unreadable");
            None
        }
    };
    (reason, since)
}

/// The user-visible notice for a rejected turn, with the operator's reason
/// appended when one was recorded.
pub fn paused_notice(reason: Option<&str>) -> String {
    match reason {
        Some(r) if !r.trim().is_empty() => format!("{PAUSED_NOTICE} — reason: {}", r.trim()),
        _ => PAUSED_NOTICE.to_string(),
    }
}

/// Edge a log-once pause gate observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseTransition {
    /// First paused observation after running (or at startup).
    Engaged,
    /// First unpaused observation after being paused.
    Released,
}

/// Log-once edge tracker for polling gates (dispatcher tick, heartbeat poll).
///
/// The gates check the flag every tick; logging every tick would bury the
/// signal. `observe` returns `Some` only on state *transitions*, so callers
/// log exactly once per pause engagement and once per release.
#[derive(Debug, Default)]
pub struct PauseGate {
    engaged: bool,
}

impl PauseGate {
    /// New gate in the not-paused state (matches the setting's default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the latest flag reading; returns the edge, if this reading is one.
    pub fn observe(&mut self, paused: bool) -> Option<PauseTransition> {
        match (self.engaged, paused) {
            (false, true) => {
                self.engaged = true;
                Some(PauseTransition::Engaged)
            }
            (true, false) => {
                self.engaged = false;
                Some(PauseTransition::Released)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_interpretation_fails_safe_on_corruption_only() {
        // Spec: missing/default = not paused; corrupt = paused. A regression
        // in either direction is dangerous — "missing → paused" would brick
        // fresh installs, "corrupt → not paused" would let a corrupted row
        // silently disable the operator's only stop lever.
        for (raw, expect, why) in [
            (None, false, "missing row is the default: not paused"),
            (Some("false"), false, "explicit false"),
            (Some("true"), true, "explicit true"),
            (Some(" true "), true, "whitespace-padded canonical value"),
            (Some("TRUE"), true, "non-canonical spelling fails safe"),
            (Some("1"), true, "numeric bool fails safe"),
            (Some("banana"), true, "garbage fails safe"),
            (Some(""), true, "empty string fails safe"),
        ] {
            assert_eq!(interpret_flag(raw), expect, "{why}: {raw:?}");
        }
    }

    #[tokio::test]
    async fn db_gate_read_honors_stored_flag_and_fails_safe() {
        let db = Database::test_db().await;
        // Never written → not paused.
        assert!(!is_engaged(&db).await, "fresh DB must not read as paused");
        db.set_setting(SETTING_KEY, "true").await.unwrap();
        assert!(is_engaged(&db).await);
        db.set_setting(SETTING_KEY, "false").await.unwrap();
        assert!(!is_engaged(&db).await);
        // Corrupt row (e.g. hand-edited DB) → fail safe to paused.
        db.set_setting(SETTING_KEY, "yes please").await.unwrap();
        assert!(
            is_engaged(&db).await,
            "corrupt value must fail safe to paused"
        );
    }

    #[test]
    fn pause_gate_reports_each_edge_exactly_once() {
        // Log-once spec: the dispatcher logs on engagement and on release,
        // never per tick. Steady-state observations must return None.
        let mut gate = PauseGate::new();
        assert_eq!(gate.observe(false), None, "startup unpaused is not an edge");
        assert_eq!(gate.observe(true), Some(PauseTransition::Engaged));
        assert_eq!(
            gate.observe(true),
            None,
            "second paused tick must not re-log"
        );
        assert_eq!(gate.observe(true), None);
        assert_eq!(gate.observe(false), Some(PauseTransition::Released));
        assert_eq!(gate.observe(false), None);
        // Re-engagement is a fresh edge.
        assert_eq!(gate.observe(true), Some(PauseTransition::Engaged));
    }

    #[test]
    fn notice_includes_reason_only_when_present() {
        assert_eq!(paused_notice(None), PAUSED_NOTICE);
        assert_eq!(paused_notice(Some("   ")), PAUSED_NOTICE);
        let with = paused_notice(Some("runaway shell loop"));
        assert!(with.starts_with(PAUSED_NOTICE));
        assert!(with.contains("runaway shell loop"));
    }
}
