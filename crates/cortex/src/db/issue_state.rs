//! Persistence for doctor/maintenance issue lifecycle state.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::Database;

/// Latest lifecycle state for a single diagnostic issue.
#[derive(Debug, Clone)]
pub struct IssueStateRow {
    /// Stable issue key, usually `Category:Name`.
    pub issue_key: String,
    /// When the issue was first seen in a non-pass state.
    pub first_seen_at: i64,
    /// Most recent observation timestamp.
    pub last_seen_at: i64,
    /// Number of consecutive sweeps the issue has remained non-pass.
    pub consecutive_count: i64,
    /// Last observed status string (`warn`, `fail`, or `resolved`).
    pub last_status: String,
    /// Number of auto-fix attempts recorded for this issue.
    pub auto_fix_attempts: i64,
    /// Unix timestamp of the most recent auto-fix attempt, if any.
    pub last_auto_fix_at: Option<i64>,
    /// Result of the most recent auto-fix attempt, if any.
    pub last_auto_fix_result: Option<String>,
    /// When the issue was last observed in a resolved state, if ever.
    pub resolved_at: Option<i64>,
}

impl Database {
    /// Load the latest lifecycle state for a single issue key.
    pub fn get_issue_state(&self, issue_key: &str) -> Result<Option<IssueStateRow>> {
        self.conn
            .query_row(
                "SELECT issue_key, first_seen_at, last_seen_at, consecutive_count,
                        last_status, auto_fix_attempts, last_auto_fix_at,
                        last_auto_fix_result, resolved_at
                 FROM maintenance_issue_state
                 WHERE issue_key = ?1",
                params![issue_key],
                |row| {
                    Ok(IssueStateRow {
                        issue_key: row.get(0)?,
                        first_seen_at: row.get(1)?,
                        last_seen_at: row.get(2)?,
                        consecutive_count: row.get(3)?,
                        last_status: row.get(4)?,
                        auto_fix_attempts: row.get(5)?,
                        last_auto_fix_at: row.get(6)?,
                        last_auto_fix_result: row.get(7)?,
                        resolved_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Upsert the lifecycle state for an issue observed this sweep.
    pub fn upsert_issue_state_observed(&self, issue_key: &str, now: i64, status: &str) -> Result<()> {
        let prior = self.get_issue_state(issue_key)?;
        let status_is_open = status != "resolved";
        let (first_seen_at, consecutive_count, auto_fix_attempts, last_auto_fix_at, last_auto_fix_result) =
            if let Some(prior) = prior {
                let next = if prior.last_seen_at == now {
                    if !status_is_open {
                        0
                    } else if prior.consecutive_count > 0 {
                        prior.consecutive_count
                    } else {
                        1
                    }
                } else if prior.last_status != "resolved" && status_is_open {
                    prior.consecutive_count + 1
                } else if !status_is_open {
                    0
                } else {
                    1
                };
                (
                    prior.first_seen_at,
                    next,
                    prior.auto_fix_attempts,
                    prior.last_auto_fix_at,
                    prior.last_auto_fix_result,
                )
            } else {
                (now, if status_is_open { 1 } else { 0 }, 0, None, None)
            };
        let resolved_at = if status_is_open { None } else { Some(now) };
        self.conn.execute(
            "INSERT INTO maintenance_issue_state
                (issue_key, first_seen_at, last_seen_at, consecutive_count, last_status,
                 auto_fix_attempts, last_auto_fix_at, last_auto_fix_result, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(issue_key) DO UPDATE SET
                first_seen_at = ?2,
                last_seen_at = ?3,
                consecutive_count = ?4,
                last_status = ?5,
                auto_fix_attempts = ?6,
                last_auto_fix_at = ?7,
                last_auto_fix_result = ?8,
                resolved_at = ?9",
            params![
                issue_key,
                first_seen_at,
                now,
                consecutive_count,
                status,
                auto_fix_attempts,
                last_auto_fix_at,
                last_auto_fix_result,
                resolved_at
            ],
        )?;
        Ok(())
    }

    /// Mark a previously-observed issue as resolved on this sweep.
    pub fn resolve_issue_state(&self, issue_key: &str, now: i64) -> Result<()> {
        let prior = self.get_issue_state(issue_key)?;
        let Some(prior) = prior else {
            return Ok(());
        };
        self.conn.execute(
            "UPDATE maintenance_issue_state
             SET last_seen_at = ?2,
                 consecutive_count = 0,
                 last_status = 'resolved',
                 resolved_at = ?2
             WHERE issue_key = ?1",
            params![issue_key, now],
        )?;
        debug_assert!(prior.first_seen_at <= now);
        Ok(())
    }

    /// Record the result of an auto-fix attempt for an issue.
    pub fn record_issue_auto_fix_attempt(
        &self,
        issue_key: &str,
        now: i64,
        result: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE maintenance_issue_state
             SET auto_fix_attempts = auto_fix_attempts + 1,
                 last_auto_fix_at = ?2,
                 last_auto_fix_result = ?3
             WHERE issue_key = ?1",
            params![issue_key, now, result],
        )?;
        Ok(())
    }

    /// Return all issues that are still open and have persisted for 2+ runs.
    pub fn list_persistent_open_issues(&self) -> Result<Vec<IssueStateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, first_seen_at, last_seen_at, consecutive_count,
                    last_status, auto_fix_attempts, last_auto_fix_at,
                    last_auto_fix_result, resolved_at
             FROM maintenance_issue_state
             WHERE last_status != 'resolved' AND consecutive_count >= 2
             ORDER BY consecutive_count DESC, issue_key ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(IssueStateRow {
                    issue_key: row.get(0)?,
                    first_seen_at: row.get(1)?,
                    last_seen_at: row.get(2)?,
                    consecutive_count: row.get(3)?,
                    last_status: row.get(4)?,
                    auto_fix_attempts: row.get(5)?,
                    last_auto_fix_at: row.get(6)?,
                    last_auto_fix_result: row.get(7)?,
                    resolved_at: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete resolved issue-state rows older than the given cutoff.
    pub fn prune_resolved_issue_state_before(&self, cutoff: i64) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM maintenance_issue_state
                 WHERE last_status = 'resolved' AND resolved_at IS NOT NULL AND resolved_at < ?1",
                params![cutoff],
            )
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_state_observation_and_resolution_lifecycle() {
        let db = Database::test_db();
        db.upsert_issue_state_observed("Embeddings:chunk coverage", 100, "warn")
            .unwrap();
        let row = db
            .get_issue_state("Embeddings:chunk coverage")
            .unwrap()
            .unwrap();
        assert_eq!(row.consecutive_count, 1);
        assert_eq!(row.last_status, "warn");

        db.upsert_issue_state_observed("Embeddings:chunk coverage", 200, "warn")
            .unwrap();
        let row = db
            .get_issue_state("Embeddings:chunk coverage")
            .unwrap()
            .unwrap();
        assert_eq!(row.consecutive_count, 2);

        db.upsert_issue_state_observed("Embeddings:chunk coverage", 300, "fail")
            .unwrap();
        let row = db
            .get_issue_state("Embeddings:chunk coverage")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.consecutive_count, 3,
            "severity flips must preserve the open-issue streak"
        );
        assert_eq!(row.last_status, "fail");

        db.record_issue_auto_fix_attempt("Embeddings:chunk coverage", 310, "fixed")
            .unwrap();
        let row = db
            .get_issue_state("Embeddings:chunk coverage")
            .unwrap()
            .unwrap();
        assert_eq!(row.auto_fix_attempts, 1);
        assert_eq!(row.last_auto_fix_result.as_deref(), Some("fixed"));

        db.resolve_issue_state("Embeddings:chunk coverage", 300)
            .unwrap();
        let row = db
            .get_issue_state("Embeddings:chunk coverage")
            .unwrap()
            .unwrap();
        assert_eq!(row.last_status, "resolved");
        assert_eq!(row.consecutive_count, 0);
        assert_eq!(row.resolved_at, Some(300));
    }

    #[test]
    fn list_persistent_open_issues_filters_resolved() {
        let db = Database::test_db();
        db.upsert_issue_state_observed("A:x", 10, "warn").unwrap();
        db.upsert_issue_state_observed("A:x", 20, "warn").unwrap();
        db.upsert_issue_state_observed("B:y", 10, "warn").unwrap();
        db.resolve_issue_state("B:y", 20).unwrap();

        let rows = db.list_persistent_open_issues().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_key, "A:x");
    }
}
