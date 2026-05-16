//! Typed cooldown registry for heartbeat augmenters.
//!
//! Replaces the inline `meta` table reads/writes that previously sprinkled
//! through `collect_matching` in `heartbeat_augmenters.rs`. Augmenters now
//! consume a `Cooldown` handle rather than parsing meta strings ad-hoc.
//!
//! Cooldown state remains persisted under `heartbeat_augmenter_last_fired_*`
//! meta keys — this is purely a representation change at the caller side.

use cortex::db::Database;

/// Per-augmenter cooldown handle. Owns one meta key.
pub(crate) struct Cooldown<'a> {
    db: &'a Database,
    key: String,
    cooldown_secs: u64,
}

impl<'a> Cooldown<'a> {
    /// Read the persisted last-fired timestamp.
    ///
    /// `Ok(Some(_))` — successfully read a stored timestamp (or absence-of-row).
    /// `Err(())` — DB read failed; caller should skip the augmenter on this
    /// tick rather than treat it as never-fired (matches the prior
    /// fail-closed behavior).
    pub(crate) async fn last_fired(&self) -> Result<u64, ()> {
        match self.db.get_meta(&self.key).await {
            Ok(Some(s)) => Ok(s.parse().unwrap_or(0)),
            Ok(None) => Ok(0),
            Err(e) => {
                tracing::warn!("heartbeat cooldowns: failed to read '{}': {e}", self.key);
                Err(())
            }
        }
    }

    /// True if `cooldown_secs` has elapsed since the last firing (or no
    /// previous firing exists). Treats DB-read failures as "do not fire".
    pub(crate) async fn is_ready(&self, now_secs: u64) -> bool {
        match self.last_fired().await {
            Ok(last) => now_secs.saturating_sub(last) >= self.cooldown_secs,
            Err(()) => false,
        }
    }

    /// Record a firing. Failures log+continue — losing this write means
    /// at most one duplicate nudge before the next persisted tick.
    pub(crate) async fn record_fired(&self, now_secs: u64) {
        if let Err(e) = self.db.set_meta(&self.key, &now_secs.to_string()).await {
            tracing::warn!("heartbeat cooldowns: failed to persist '{}': {e}", self.key);
        }
    }
}

/// Registry resolving augmenter ids to typed [`Cooldown`] handles.
///
/// The table itself is stateless — it just routes `(id, cooldown_secs)`
/// pairs to the right meta key. Persistence still happens through the
/// shared `Database`.
pub(crate) struct CooldownTable<'a> {
    db: &'a Database,
}

impl<'a> CooldownTable<'a> {
    pub(crate) fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Return a typed handle for the augmenter named `id`.
    pub(crate) fn for_augmenter(&self, id: &str, cooldown_secs: u64) -> Cooldown<'_> {
        Cooldown {
            db: self.db,
            key: meta_key_for(id),
            cooldown_secs,
        }
    }
}

/// Public so tests in `heartbeat_augmenters.rs` that pre-seed cooldown
/// state can compute the same key the production code uses.
pub(crate) fn meta_key_for(id: &str) -> String {
    format!("heartbeat_augmenter_last_fired_{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn cooldown_is_ready_when_never_fired() {
        let db = db().await;
        let table = CooldownTable::new(&db);
        let cd = table.for_augmenter("test_aug", 60);
        assert!(
            cd.is_ready(1_000).await,
            "never-fired cooldown must be ready"
        );
    }

    #[tokio::test]
    async fn cooldown_blocks_within_window_and_releases_after() {
        let db = db().await;
        let table = CooldownTable::new(&db);
        let cd = table.for_augmenter("test_aug", 100);

        cd.record_fired(1_000).await;
        assert!(!cd.is_ready(1_050).await, "still inside cooldown window");
        assert!(
            cd.is_ready(1_100).await,
            "exactly at cooldown boundary releases"
        );
        assert!(cd.is_ready(1_200).await, "well past cooldown window");
    }

    #[tokio::test]
    async fn cooldown_skips_when_meta_table_unreadable() {
        // Drop the meta table so reads fail — we must NOT treat that as
        // "never fired" (which would spam the user); we must skip.
        let db = db().await;
        db.conn()
            .execute_batch("DROP TABLE IF EXISTS meta")
            .await
            .expect("drop meta");
        let table = CooldownTable::new(&db);
        let cd = table.for_augmenter("test_aug", 60);
        assert!(
            !cd.is_ready(1_000).await,
            "DB read failure must fail-closed (do not fire)"
        );
    }

    #[test]
    fn meta_keys_are_namespaced_per_augmenter_id() {
        // Regression: two augmenters must not share a key, or one's
        // cooldown would silently suppress the other.
        assert_ne!(meta_key_for("a"), meta_key_for("b"));
        assert!(meta_key_for("a").contains("heartbeat_augmenter_last_fired_"));
    }
}
