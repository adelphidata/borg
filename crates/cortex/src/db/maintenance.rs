//! DB helpers for the self-healing maintenance subsystem.
//!
//! Backed by the `doctor_runs` table. Kept separate from `tasks.rs`
//! so the maintenance module's persistence surface is easy to audit.

use super::Database;
use anyhow::Result;
use libsql::params;

use crate::maintenance::MaintenanceReport;

impl Database {
    /// Append a completed maintenance run to `doctor_runs`. Stores pass/warn/fail
    /// counts denormalized for cheap dashboard queries plus the full report
    /// JSON for diffing across runs.
    pub async fn record_doctor_run(&self, report: &MaintenanceReport) -> Result<()> {
        let json = serde_json::to_string(report)?;
        self.conn
            .execute(
                "INSERT INTO doctor_runs (ran_at, pass_count, warn_count, fail_count, report_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params!(
                    report.ran_at,
                    report.pass_count as i64,
                    report.warn_count as i64,
                    report.fail_count as i64,
                    json,
                ),
            )
            .await?;
        Ok(())
    }

    /// Return the most recently recorded maintenance report, or None.
    pub async fn latest_doctor_run(&self) -> Result<Option<MaintenanceReport>> {
        let mut rows = self
            .conn
            .query(
                "SELECT report_json FROM doctor_runs ORDER BY ran_at DESC, id DESC LIMIT 1",
                (),
            )
            .await?;
        match rows.next().await? {
            Some(row) => {
                let j: String = row.get::<String>(0)?;
                Ok(Some(serde_json::from_str(&j)?))
            }
            None => Ok(None),
        }
    }

    /// Keep the `keep` most recent rows in `doctor_runs`, delete the rest.
    /// Returns number of rows deleted.
    pub async fn prune_doctor_runs(&self, keep: usize) -> Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM doctor_runs WHERE id NOT IN (
                    SELECT id FROM doctor_runs ORDER BY ran_at DESC, id DESC LIMIT ?1
                )",
                params!(keep as i64),
            )
            .await?;
        Ok(deleted as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(ran_at: i64, pass: usize, warn: usize, fail: usize) -> MaintenanceReport {
        MaintenanceReport {
            ran_at,
            pass_count: pass,
            warn_count: warn,
            fail_count: fail,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn latest_doctor_run_is_none_when_empty() {
        let db = Database::test_db().await;
        assert!(db.latest_doctor_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_then_latest_round_trips_via_json() {
        // Spec: the JSON column is the source of truth for the report —
        // a regression that stored only the count columns would silently
        // lose the persistent_warnings / check_summary / vitals fields
        // that the TUI's diff view depends on.
        let db = Database::test_db().await;
        let mut r = report(1_700_000_000, 5, 2, 1);
        r.persistent_warnings = vec!["Memory:Embedding cache stale".to_string()];
        r.check_summary = vec!["PASS Foo:Bar".to_string(), "FAIL Baz:Qux".to_string()];
        r.vacuum_ran = true;
        r.vacuum_bytes_reclaimed = 4096;
        db.record_doctor_run(&r).await.unwrap();
        let got = db
            .latest_doctor_run()
            .await
            .unwrap()
            .expect("latest must return Some after record");
        assert_eq!(got.ran_at, r.ran_at);
        assert_eq!(got.pass_count, r.pass_count);
        assert_eq!(got.persistent_warnings, r.persistent_warnings);
        assert_eq!(got.check_summary, r.check_summary);
        assert!(got.vacuum_ran);
        assert_eq!(got.vacuum_bytes_reclaimed, 4096);
    }

    #[tokio::test]
    async fn latest_picks_most_recent_by_ran_at_then_id() {
        // Real-world failure: two reports recorded in the same second
        // (ran_at equal). The ORDER BY ran_at DESC, id DESC tiebreaker
        // must pick the most-recently-inserted. A regression that dropped
        // the id tiebreaker would non-determine the answer.
        let db = Database::test_db().await;
        db.record_doctor_run(&report(100, 1, 0, 0)).await.unwrap();
        db.record_doctor_run(&report(200, 2, 0, 0)).await.unwrap();
        db.record_doctor_run(&report(200, 3, 0, 0)).await.unwrap(); // same ran_at, newer id
        let latest = db.latest_doctor_run().await.unwrap().unwrap();
        assert_eq!(latest.ran_at, 200);
        assert_eq!(
            latest.pass_count, 3,
            "newer id within same ran_at must win"
        );
    }

    #[tokio::test]
    async fn prune_keeps_only_n_most_recent() {
        let db = Database::test_db().await;
        for i in 0..5 {
            db.record_doctor_run(&report(100 + i, 0, 0, 0))
                .await
                .unwrap();
        }
        // Keep top 2 → 3 rows deleted.
        let deleted = db.prune_doctor_runs(2).await.unwrap();
        assert_eq!(deleted, 3);
        // And the surviving rows must be the most recent ones.
        let latest = db.latest_doctor_run().await.unwrap().unwrap();
        assert_eq!(latest.ran_at, 104);
    }

    #[tokio::test]
    async fn prune_zero_deletes_every_row() {
        // Edge: `keep=0` must wipe the table. A subtle off-by-one (e.g.
        // LIMIT clause that rejects 0) would silently leave rows around.
        let db = Database::test_db().await;
        db.record_doctor_run(&report(100, 0, 0, 0)).await.unwrap();
        db.record_doctor_run(&report(200, 0, 0, 0)).await.unwrap();
        let deleted = db.prune_doctor_runs(0).await.unwrap();
        assert_eq!(deleted, 2);
        assert!(db.latest_doctor_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prune_with_keep_larger_than_count_is_a_no_op() {
        // Spec: asking to keep more rows than exist must not error and
        // must not delete anything. Operator setting `doctor_runs_keep`
        // higher than current row count shouldn't trip a panic.
        let db = Database::test_db().await;
        db.record_doctor_run(&report(100, 0, 0, 0)).await.unwrap();
        let deleted = db.prune_doctor_runs(100).await.unwrap();
        assert_eq!(deleted, 0);
        assert!(db.latest_doctor_run().await.unwrap().is_some());
    }
}
