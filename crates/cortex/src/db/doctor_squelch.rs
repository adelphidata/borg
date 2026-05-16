//! User-managed squelch list for doctor findings.
//!
//! A squelch is a `(category, name)` pair the user has explicitly chosen
//! to suppress from the heartbeat security nudge and the daily maintenance
//! sweep. Squelches expire after a TTL (configurable, default 30 days) so
//! a once-acknowledged risk doesn't disappear from the radar permanently —
//! the daily sweep prunes expired rows and the warning re-surfaces.

use anyhow::Result;
use turso::params;

use super::Database;

/// One active squelch row.
#[derive(Debug, Clone)]
pub struct DoctorSquelchRow {
    pub category: String,
    pub name: String,
    pub squelched_at: i64,
    pub expires_at: i64,
    pub reason: Option<String>,
}

impl Database {
    /// Mark a finding as squelched until `expires_at`. Re-squelching an
    /// existing row refreshes the timestamps and reason.
    pub async fn doctor_squelch_set(
        &self,
        category: &str,
        name: &str,
        squelched_at: i64,
        expires_at: i64,
        reason: Option<&str>,
    ) -> Result<()> {
        let reason_owned = reason.map(std::string::ToString::to_string);
        self.conn
            .execute(
                "INSERT INTO doctor_squelches (category, name, squelched_at, expires_at, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (category, name) DO UPDATE SET
                squelched_at = excluded.squelched_at,
                expires_at = excluded.expires_at,
                reason = excluded.reason",
                params!(category, name, squelched_at, expires_at, reason_owned),
            )
            .await?;
        Ok(())
    }

    /// Remove a squelch by `(category, name)`. Returns whether a row was
    /// removed — false means no active squelch existed.
    pub async fn doctor_squelch_clear(&self, category: &str, name: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM doctor_squelches WHERE category = ?1 AND name = ?2",
                params!(category, name),
            )
            .await?;
        Ok(n > 0)
    }

    /// All squelches whose `expires_at > now`. Callers filter their
    /// diagnostic checks against this list before surfacing.
    pub async fn doctor_squelch_list_active(&self, now: i64) -> Result<Vec<DoctorSquelchRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT category, name, squelched_at, expires_at, reason
             FROM doctor_squelches
             WHERE expires_at > ?1",
            )
            .await?;
        let mut rows = stmt.query(params!(now)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(DoctorSquelchRow {
                category: row.get::<String>(0)?,
                name: row.get::<String>(1)?,
                squelched_at: row.get::<i64>(2)?,
                expires_at: row.get::<i64>(3)?,
                reason: row.get::<Option<String>>(4)?,
            });
        }
        Ok(out)
    }

    /// Delete every squelch whose `expires_at <= now`. Returns the row
    /// count so the daily maintenance report can surface activity.
    pub async fn doctor_squelch_prune_expired(&self, now: i64) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "DELETE FROM doctor_squelches WHERE expires_at <= ?1",
                params!(now),
            )
            .await?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn squelch_round_trip_and_replace() {
        let db = db().await;
        db.doctor_squelch_set(
            "Host Security",
            "Application Firewall",
            100,
            200,
            Some("ack"),
        )
        .await
        .unwrap();
        let active = db.doctor_squelch_list_active(150).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].category, "Host Security");
        assert_eq!(active[0].name, "Application Firewall");
        assert_eq!(active[0].expires_at, 200);
        assert_eq!(active[0].reason.as_deref(), Some("ack"));

        // Re-squelching the same key updates instead of duplicating —
        // the unique (category, name) PK enforces this.
        db.doctor_squelch_set("Host Security", "Application Firewall", 300, 400, None)
            .await
            .unwrap();
        let active = db.doctor_squelch_list_active(350).await.unwrap();
        assert_eq!(active.len(), 1, "must replace, not duplicate");
        assert_eq!(active[0].squelched_at, 300);
        assert_eq!(active[0].expires_at, 400);
        assert!(active[0].reason.is_none());
    }

    #[tokio::test]
    async fn list_active_excludes_expired() {
        let db = db().await;
        db.doctor_squelch_set("Host Security", "ALF", 100, 200, None)
            .await
            .unwrap();
        db.doctor_squelch_set("Host Security", "PF", 100, 500, None)
            .await
            .unwrap();
        // now=300 → only PF (expires 500) is active; ALF (expires 200) is not.
        let active = db.doctor_squelch_list_active(300).await.unwrap();
        let names: Vec<_> = active.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["PF"]);
    }

    #[tokio::test]
    async fn clear_removes_only_specified_row() {
        let db = db().await;
        db.doctor_squelch_set("Host Security", "ALF", 100, 999, None)
            .await
            .unwrap();
        db.doctor_squelch_set("Host Security", "PF", 100, 999, None)
            .await
            .unwrap();
        assert!(db
            .doctor_squelch_clear("Host Security", "ALF")
            .await
            .unwrap());
        let active = db.doctor_squelch_list_active(200).await.unwrap();
        let names: Vec<_> = active.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["PF"]);

        // Clearing an absent row is a no-op (returns false), not an error.
        assert!(!db
            .doctor_squelch_clear("Host Security", "ALF")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn prune_expired_removes_only_expired_rows() {
        let db = db().await;
        db.doctor_squelch_set("Host Security", "expired", 100, 200, None)
            .await
            .unwrap();
        db.doctor_squelch_set("Host Security", "still_live", 100, 999, None)
            .await
            .unwrap();
        // now=300 → "expired" (expires 200) gets pruned; "still_live" stays.
        let pruned = db.doctor_squelch_prune_expired(300).await.unwrap();
        assert_eq!(pruned, 1);
        let active = db.doctor_squelch_list_active(300).await.unwrap();
        let names: Vec<_> = active.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["still_live"]);
    }

    #[tokio::test]
    async fn prune_at_exact_expiry_removes_row() {
        // expires_at <= now is the deletion condition — at the boundary
        // the row is considered expired. Documents the edge case so a
        // future `<` typo would fail loudly.
        let db = db().await;
        db.doctor_squelch_set("Host Security", "boundary", 100, 200, None)
            .await
            .unwrap();
        let pruned = db.doctor_squelch_prune_expired(200).await.unwrap();
        assert_eq!(pruned, 1);
        assert!(db.doctor_squelch_list_active(0).await.unwrap().is_empty());
    }
}
