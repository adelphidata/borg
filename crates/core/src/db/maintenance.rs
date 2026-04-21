//! DB helpers for the self-healing maintenance subsystem.
//!
//! Backed by the V37 `doctor_runs` table. Kept separate from `tasks.rs`
//! so the maintenance module's persistence surface is easy to audit.

use super::Database;
use anyhow::Result;
use rusqlite::params;
use rusqlite::OptionalExtension;

use crate::maintenance::MaintenanceReport;

impl Database {
    /// Append a completed maintenance run to `doctor_runs`. Stores pass/warn/fail
    /// counts denormalized for cheap dashboard queries plus the full report
    /// JSON for diffing across runs.
    pub fn record_doctor_run(&self, report: &MaintenanceReport) -> Result<()> {
        let json = serde_json::to_string(report)?;
        self.conn.execute(
            "INSERT INTO doctor_runs (ran_at, pass_count, warn_count, fail_count, report_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                report.ran_at,
                report.pass_count as i64,
                report.warn_count as i64,
                report.fail_count as i64,
                json,
            ],
        )?;
        Ok(())
    }

    /// Return the most recently recorded maintenance report, or None.
    pub fn latest_doctor_run(&self) -> Result<Option<MaintenanceReport>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT report_json FROM doctor_runs ORDER BY ran_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    /// Keep the `keep` most recent rows in `doctor_runs`, delete the rest.
    /// Returns number of rows deleted.
    pub fn prune_doctor_runs(&self, keep: usize) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM doctor_runs WHERE id NOT IN (
                SELECT id FROM doctor_runs ORDER BY ran_at DESC LIMIT ?1
            )",
            params![keep as i64],
        )?;
        Ok(deleted)
    }

    /// Count stalled scheduled tasks auto-healed within the last
    /// `window_secs` seconds — surfaced by `borg doctor` / `/status`.
    pub fn count_healed_tasks_since(&self, since_ts: i64) -> Result<i64> {
        self.count_missed_runs_since(since_ts)
    }
}
