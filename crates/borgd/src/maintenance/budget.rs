//! Typed budget handle consumed by the heartbeat `budget_threshold`
//! augmenter.
//!
//! Wraps the meta-table bucket bookkeeping that the augmenter previously
//! implemented inline. Augmenter consumes this typed handle rather than
//! parsing meta-table strings directly.

use cortex::db::Database;

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
