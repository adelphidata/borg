//! Data-handling configuration: retention and session archiving.
//!
//! Retention is daemon-wide and single-knob (`retention_days`). The daily
//! maintenance task purges messages and tombstones audit-chain rows whose
//! `ts` falls past the cutoff. A *pinned* session is the per-session escape
//! hatch from that purge — `purge_policy = Never` is all-or-nothing.
//!
//! `session_auto_archive_days` is a separate, opt-in sweep that soft-hides
//! sessions with no recent activity. It hides, never deletes.
//!
//! Backups are tarball-based and manual only: `borg backup create / verify
//! / restore`.

use serde::{Deserialize, Serialize};

/// Retention configuration. All fields stored under `data.*` keys in the
/// SQLite settings table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DataConfig {
    /// How many days of `messages` and audit-chain rows to keep. `None`
    /// (the default) means **forever** — purge skipped entirely. Setting
    /// to e.g. `Some(90)` enables daily purge of older rows.
    pub retention_days: Option<u32>,
    /// Whether the daily purge job runs automatically.
    pub purge_policy: PurgePolicy,
    /// Archive sessions with no activity for this many days. `None` (the
    /// default) disables the sweep entirely.
    ///
    /// Archiving is a soft hide, never a delete, and it ages on last activity
    /// rather than creation — so an old-but-still-used conversation is spared.
    /// Pinned sessions are always exempt.
    pub session_auto_archive_days: Option<u32>,
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

impl std::fmt::Display for PurgePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::OnDemandOnly => "on_demand_only",
            Self::Never => "never",
        })
    }
}

impl std::str::FromStr for PurgePolicy {
    type Err = anyhow::Error;

    /// Parses the same spellings [`Display`] emits, so a value round-trips
    /// through the settings table. Hyphens are accepted as a convenience —
    /// `on-demand-only` is the natural thing to type on a CLI.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "auto" => Ok(Self::Auto),
            "on_demand_only" => Ok(Self::OnDemandOnly),
            "never" => Ok(Self::Never),
            other => anyhow::bail!(
                "invalid purge policy '{other}' (expected auto, on_demand_only, or never)"
            ),
        }
    }
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            retention_days: None,
            purge_policy: PurgePolicy::Auto,
            session_auto_archive_days: None,
        }
    }
}

impl DataConfig {
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
        days_cutoff(self.retention_days, now_ts)
    }

    /// Last-activity cutoff for the auto-archive sweep. `None` when the sweep
    /// is disabled, which is the default.
    ///
    /// Independent of `purge_policy`: archiving hides, it never deletes, so
    /// the litigation-hold switch that stops deletion has no reason to stop
    /// tidying a session list.
    pub fn session_archive_cutoff(&self, now_ts: i64) -> Option<i64> {
        days_cutoff(self.session_auto_archive_days, now_ts)
    }
}

/// `now_ts - days`, or `None` when `days` is unset or zero.
///
/// Zero is "disabled", not "everything is older than now" — a day count that
/// round-trips through settings as 0 must never become a cutoff that matches
/// every row.
fn days_cutoff(days: Option<u32>, now_ts: i64) -> Option<i64> {
    days.filter(|d| *d > 0)
        .map(|d| now_ts - (d as i64) * crate::constants::SECS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults are forever-retention with auto-purge ready to go once
    /// retention_days is set. A regression that flips `retention_days` to
    /// `Some(N)` would silently start deleting user data on upgrade —
    /// guard against it.
    #[test]
    fn defaults_match_locked_spec() {
        let c = DataConfig::default();
        assert_eq!(c.retention_days, None);
        assert_eq!(c.purge_policy, PurgePolicy::Auto);
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
            ..DataConfig::default()
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
            ..DataConfig::default()
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
            ..DataConfig::default()
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
            ..DataConfig::default()
        };
        assert!(c.should_auto_purge());
        assert_eq!(c.purge_cutoff(0), Some(-30 * 86_400));
    }

    /// Archiving is off unless the operator asks for it, and `Some(0)` is
    /// treated as unset rather than "archive everything right now" — the same
    /// typo-guard `retention_days` gets, and here it would hide every session
    /// in the list at once.
    #[test]
    fn session_archive_is_opt_in_and_zero_days_is_a_no_op() {
        assert!(DataConfig::default()
            .session_archive_cutoff(1_700_000_000)
            .is_none());

        let zero = DataConfig {
            session_auto_archive_days: Some(0),
            ..DataConfig::default()
        };
        assert!(zero.session_archive_cutoff(1_700_000_000).is_none());

        let thirty = DataConfig {
            session_auto_archive_days: Some(30),
            ..DataConfig::default()
        };
        assert_eq!(thirty.session_archive_cutoff(0), Some(-30 * 86_400));
    }
}
