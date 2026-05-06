//! Typed budget handles consumed by augmenters and the daily sweep driver.
//!
//! Two independent budgets live here:
//!
//! 1. [`ActionBudget`] — caps the number of *mutating heal actions* one daily
//!    sweep is allowed to perform. Augmenters and the sweep driver consume this
//!    instead of reading `maintenance.heal_action_budget` ad-hoc.
//! 2. [`MonthlyBudgetTracker`] — wraps the meta-table bucket bookkeeping that
//!    the heartbeat `budget_threshold` augmenter previously implemented inline.
//!    The augmenter now consumes this typed handle rather than parsing
//!    meta-table strings directly.
//!
//! Per `RUST-IDIOMS.md`: narrow refs over fat contexts. Each consumer takes
//! exactly the handle it needs — no `MaintenanceContext` god-struct.

use cortex::db::Database;
use thiserror::Error;

/// Budget exhausted — no remaining capacity to consume.
#[derive(Debug, Error)]
#[error("heal action budget exhausted ({spent}/{limit})")]
pub(crate) struct BudgetExhausted {
    pub spent: u32,
    pub limit: u32,
}

/// Heal-action budget for one sweep. Caps how many mutating actions a sweep
/// may perform before later steps short-circuit.
///
/// Across consecutive sweeps that exhaust their budget, [`Self::record_sweep_end`]
/// bumps `exhausted_streak`. The `Maintenance` driver surfaces a persistent
/// warning after two consecutive exhausts (matches the
/// `maintenance.heal_action_budget` invariant in `CLAUDE.md`).
#[derive(Debug, Clone)]
pub(crate) struct ActionBudget {
    limit: u32,
    spent: u32,
    exhausted: bool,
    exhausted_streak: u32,
}

impl ActionBudget {
    /// Construct with a fresh limit. The limit is read once at module
    /// construction (from `maintenance.heal_action_budget`) and never
    /// re-read at consumer call sites.
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            limit,
            spent: 0,
            exhausted: false,
            exhausted_streak: 0,
        }
    }

    /// Reset spent/exhausted at the start of a new sweep. The streak is
    /// preserved across sweeps — that's the whole point of the streak
    /// counter.
    pub(crate) fn begin_sweep(&mut self) {
        self.spent = 0;
        self.exhausted = false;
    }

    /// Try to consume `n` actions. Returns `Err(BudgetExhausted)` once the
    /// limit is reached and stays in that state for the rest of the sweep.
    pub(crate) fn try_consume(&mut self, n: u32) -> Result<(), BudgetExhausted> {
        if self.exhausted {
            return Err(BudgetExhausted {
                spent: self.spent,
                limit: self.limit,
            });
        }
        self.spent = self.spent.saturating_add(n);
        if self.spent >= self.limit {
            self.exhausted = true;
            return Err(BudgetExhausted {
                spent: self.spent,
                limit: self.limit,
            });
        }
        Ok(())
    }

    /// True if the budget has been exhausted in the current sweep.
    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Number of consecutive sweeps that have exhausted the budget.
    pub(crate) fn exhausted_streak(&self) -> u32 {
        self.exhausted_streak
    }

    /// Configured limit (read once at construction).
    pub(crate) fn limit(&self) -> u32 {
        self.limit
    }

    /// Actions consumed this sweep.
    pub(crate) fn spent(&self) -> u32 {
        self.spent
    }

    /// Account for the just-finished sweep. Bumps `exhausted_streak` if the
    /// sweep ran out of budget; otherwise resets the streak to zero.
    pub(crate) fn record_sweep_end(&mut self, sweep_was_exhausted: bool) {
        if sweep_was_exhausted {
            self.exhausted_streak = self.exhausted_streak.saturating_add(1);
        } else {
            self.exhausted_streak = 0;
        }
    }
}

/// Bucket the heartbeat `budget_threshold` augmenter has already announced
/// for the current calendar month. Wraps the meta-table reads/writes that
/// the augmenter previously did inline against `heartbeat_budget_last_bucket_*`.
pub(crate) struct MonthlyBudgetTracker<'a> {
    db: &'a Database,
}

impl<'a> MonthlyBudgetTracker<'a> {
    pub(crate) fn new(db: &'a Database) -> Self {
        Self { db }
    }

    fn month_key(now: chrono::DateTime<chrono::Utc>) -> String {
        use chrono::Datelike;
        format!(
            "heartbeat_budget_last_bucket_{:04}-{:02}",
            now.year(),
            now.month(),
        )
    }

    /// Read the last-announced bucket for the current month. Returns `0`
    /// when no bucket has been persisted yet (or when meta access fails —
    /// "behave as if never announced" matches the augmenter's prior
    /// behavior).
    pub(crate) fn last_bucket(&self, now: chrono::DateTime<chrono::Utc>) -> u8 {
        let key = Self::month_key(now);
        match self.db.get_meta(&key) {
            Ok(Some(s)) => s.parse().unwrap_or(0),
            Ok(None) => 0,
            Err(e) => {
                tracing::warn!("monthly_budget: failed to read {key}: {e}");
                0
            }
        }
    }

    /// Persist the most-recently-announced bucket so the same threshold
    /// doesn't re-fire within the calendar month. Logs and continues on
    /// failure — losing this write means at most one duplicate nudge.
    pub(crate) fn record_bucket(&self, now: chrono::DateTime<chrono::Utc>, bucket: u8) {
        let key = Self::month_key(now);
        if let Err(e) = self.db.set_meta(&key, &bucket.to_string()) {
            tracing::warn!("monthly_budget: failed to persist bucket {bucket}: {e}");
        }
    }

    /// Decide whether `bucket` is a new high for the current month and, if
    /// so, persist it. Returns `true` when the augmenter should fire.
    pub(crate) fn should_announce(&self, now: chrono::DateTime<chrono::Utc>, bucket: u8) -> bool {
        let last = self.last_bucket(now);
        if bucket <= last {
            return false;
        }
        self.record_bucket(now, bucket);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_budget_starts_unspent_and_unexhausted() {
        let b = ActionBudget::new(100);
        assert_eq!(b.limit(), 100);
        assert_eq!(b.spent(), 0);
        assert!(!b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 0);
    }

    #[test]
    fn try_consume_within_limit_succeeds_and_tracks_spend() {
        let mut b = ActionBudget::new(100);
        b.try_consume(40).expect("under limit");
        assert_eq!(b.spent(), 40);
        b.try_consume(10).expect("still under limit");
        assert_eq!(b.spent(), 50);
        assert!(!b.is_exhausted());
    }

    #[test]
    fn try_consume_returns_err_at_limit_and_stays_exhausted() {
        let mut b = ActionBudget::new(10);
        assert!(
            b.try_consume(10).is_err(),
            "spending exactly the limit must exhaust"
        );
        assert!(b.is_exhausted(), "exhausted state must persist");
        let err = b.try_consume(1).expect_err("further consume must error");
        assert_eq!(err.limit, 10);
    }

    #[test]
    fn record_sweep_end_increments_streak_on_exhaust() {
        let mut b = ActionBudget::new(5);
        // First sweep: exhaust.
        let _ = b.try_consume(10);
        b.record_sweep_end(b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 1);

        // Second sweep starts fresh but also exhausts — streak goes to 2.
        b.begin_sweep();
        let _ = b.try_consume(10);
        b.record_sweep_end(b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 2);
    }

    #[test]
    fn record_sweep_end_resets_streak_on_clean_sweep() {
        let mut b = ActionBudget::new(5);
        let _ = b.try_consume(10); // exhaust
        b.record_sweep_end(b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 1);

        // Clean sweep — streak resets.
        b.begin_sweep();
        b.try_consume(2).expect("under limit");
        b.record_sweep_end(b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 0);
    }

    #[test]
    fn begin_sweep_clears_spent_and_exhausted_but_preserves_streak() {
        let mut b = ActionBudget::new(5);
        let _ = b.try_consume(10);
        b.record_sweep_end(b.is_exhausted());
        assert_eq!(b.exhausted_streak(), 1);

        b.begin_sweep();
        assert_eq!(b.spent(), 0, "spent must reset");
        assert!(!b.is_exhausted(), "exhausted must reset");
        assert_eq!(
            b.exhausted_streak(),
            1,
            "streak must persist across sweep boundary"
        );
    }
}
