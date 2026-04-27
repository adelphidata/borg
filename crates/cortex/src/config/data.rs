//! Data-handling configuration: retention + backup.
//!
//! Retention is daemon-wide and single-knob (`retention_days`). The daily
//! maintenance task purges messages and tombstones audit-chain rows whose
//! `ts` falls past the cutoff.
//!
//! Backups are tarball-based (`borg backup create/verify/restore`); a
//! weekly auto-backup runs from the same maintenance task and prunes to
//! `backup_keep_count`.

use serde::{Deserialize, Serialize};

/// Retention + backup configuration. All fields stored under `data.*` keys
/// in the SQLite settings table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DataConfig {
    /// How many days of `messages` and audit-chain rows to keep. `None`
    /// (the default) means **forever** — purge skipped entirely. Setting
    /// to e.g. `Some(90)` enables daily purge of older rows.
    pub retention_days: Option<u32>,
    /// Whether the daily purge job runs automatically.
    pub purge_policy: PurgePolicy,
    /// Number of weekly auto-backups to retain in `~/.borg/backups/`.
    /// Range `0..=52`. `0` disables auto-backup. Default `3`.
    pub backup_keep_count: u8,
}

/// When the retention purge job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgePolicy {
    /// Daily maintenance task auto-purges anything past `retention_days`.
    Auto,
    /// Daily job skipped; user runs `borg admin purge` manually.
    OnDemandOnly,
    /// Purge entirely disabled even when `retention_days` is set —
    /// litigation-hold escape hatch.
    Never,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            retention_days: None,
            purge_policy: PurgePolicy::Auto,
            backup_keep_count: 3,
        }
    }
}

impl DataConfig {
    /// Maximum allowed value for `backup_keep_count`. Values outside the
    /// range are rejected by the settings resolver.
    pub const BACKUP_KEEP_COUNT_MAX: u8 = 52;

    /// True when the purge job should actually run on the daily sweep.
    /// Encapsulates the `(retention_days, purge_policy)` interaction so
    /// callers don't reinvent the logic and miss the `Never` short-circuit.
    pub fn should_auto_purge(&self) -> bool {
        matches!(
            (self.retention_days, self.purge_policy),
            (Some(days), PurgePolicy::Auto) if days > 0
        )
    }

    /// Cutoff timestamp (`now - retention_days * 86400`). Returns `None`
    /// when retention is unlimited.
    pub fn purge_cutoff(&self, now_ts: i64) -> Option<i64> {
        self.retention_days
            .filter(|d| *d > 0)
            .map(|d| now_ts - (d as i64) * 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults are forever-retention with auto-purge ready to go once
    /// retention_days is set, and 3 backups kept. A regression that flips
    /// `retention_days` to `Some(N)` would silently start deleting user
    /// data on upgrade — guard against it.
    #[test]
    fn defaults_match_locked_spec() {
        let c = DataConfig::default();
        assert_eq!(c.retention_days, None);
        assert_eq!(c.purge_policy, PurgePolicy::Auto);
        assert_eq!(c.backup_keep_count, 3);
        assert!(!c.should_auto_purge(), "no retention → no purge");
        assert!(c.purge_cutoff(1_700_000_000).is_none());
    }

    /// `Never` is a hard block — purge never runs even with a finite
    /// `retention_days`. Matches `data-handling.md` decision #1's
    /// litigation-hold escape hatch.
    #[test]
    fn never_blocks_purge_even_with_retention_set() {
        let c = DataConfig {
            retention_days: Some(1),
            purge_policy: PurgePolicy::Never,
            backup_keep_count: 3,
        };
        assert!(!c.should_auto_purge());
    }

    /// `OnDemandOnly` skips the *daily* job; the value still resolves a
    /// cutoff so a manual `borg admin purge --before <date>` can use it.
    #[test]
    fn on_demand_only_skips_daily_but_still_yields_cutoff() {
        let c = DataConfig {
            retention_days: Some(7),
            purge_policy: PurgePolicy::OnDemandOnly,
            backup_keep_count: 3,
        };
        assert!(!c.should_auto_purge());
        assert_eq!(c.purge_cutoff(700_000), Some(700_000 - 7 * 86_400));
    }

    /// `retention_days = Some(0)` would mean "purge everything immediately"
    /// — almost certainly a config typo for `None`. Treat as no-op.
    #[test]
    fn zero_retention_days_is_treated_as_unlimited() {
        let c = DataConfig {
            retention_days: Some(0),
            purge_policy: PurgePolicy::Auto,
            backup_keep_count: 3,
        };
        assert!(!c.should_auto_purge());
        assert!(c.purge_cutoff(1_700_000_000).is_none());
    }

    /// Auto + finite retention → cutoff is `now - days * 86400`.
    #[test]
    fn auto_with_finite_retention_yields_cutoff() {
        let c = DataConfig {
            retention_days: Some(30),
            purge_policy: PurgePolicy::Auto,
            backup_keep_count: 3,
        };
        assert!(c.should_auto_purge());
        assert_eq!(c.purge_cutoff(0), Some(-30 * 86_400));
    }
}
