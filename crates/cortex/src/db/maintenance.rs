//! DB helpers for the self-healing maintenance subsystem.
//!
//! Backed by the `doctor_runs` table. Kept separate from `tasks.rs`
//! so the maintenance module's persistence surface is easy to audit.

use super::Database;
use anyhow::Result;
use turso::params;

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
