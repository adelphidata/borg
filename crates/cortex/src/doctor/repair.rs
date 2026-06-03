//! On-demand repair driver for `borg doctor --repair` / `--fix`.
//!
//! Applies the same `AutoSafe` remediations the nightly self-heal sweep
//! applies — via the shared [`crate::maintenance::apply_remediation_key`] —
//! but as an operator-triggered action that takes a full `BORG_HOME` backup
//! first. Self-heal is the cron surface; this is the on-demand surface.
//!
//! Scope is deliberately narrow (the `AutoSafe` set: runtime-subdir
//! recreation, `global/INDEX` seeding, embedding backfill, and orphan
//! daemon-lock removal). Guarded/destructive repairs (transcript archival,
//! config/auth migration) are intentionally not wired here yet.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::doctor::{DiagnosticReport, RemediationKind};

/// Outcome of a `borg doctor --repair` run.
#[derive(Debug, Default)]
pub struct RepairReport {
    /// Backup tarball written before mutating, when any repair ran.
    pub backup_path: Option<PathBuf>,
    /// Remediation keys that mutated state.
    pub applied: Vec<String>,
    /// Remediation keys that ran but changed nothing (already healthy).
    pub noop: Vec<String>,
    /// `(key, error)` for remediations that failed.
    pub failed: Vec<(String, String)>,
}

impl RepairReport {
    /// True when no remediation was attempted (nothing actionable was found).
    pub fn nothing_to_do(&self) -> bool {
        self.applied.is_empty() && self.noop.is_empty() && self.failed.is_empty()
    }
}

/// Distinct `AutoSafe` remediation keys named by `report`, in first-seen
/// order. Pure — the selection logic is unit-tested here without touching the
/// DB or filesystem.
pub fn autosafe_keys(report: &DiagnosticReport) -> Vec<&'static str> {
    let mut seen = BTreeSet::new();
    let mut keys = Vec::new();
    for check in &report.checks {
        if check.remediation_kind != RemediationKind::AutoSafe {
            continue;
        }
        if let Some(key) = check.remediation_key {
            if seen.insert(key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// Apply every `AutoSafe` remediation named by `report`, taking a full-home
/// backup first. `force` lets the run proceed when the pre-repair backup can't
/// be written (e.g. a read-only `backups/`); without it a backup failure
/// aborts before any mutation. Returns early with an empty report — and takes
/// **no** backup — when nothing is repairable.
pub async fn repair_from_report(
    db: &Database,
    config: &Config,
    report: &DiagnosticReport,
    force: bool,
) -> Result<RepairReport> {
    let keys = autosafe_keys(report);
    let mut out = RepairReport::default();
    if keys.is_empty() {
        return Ok(out);
    }

    // Backup-first: reuse the same machinery as `borg backup`. On failure,
    // abort unless --force so a repair never silently mutates without a
    // recovery point.
    match take_backup().await {
        Ok(path) => out.backup_path = Some(path),
        Err(e) => {
            if !force {
                anyhow::bail!(
                    "pre-repair backup failed: {e}. Re-run with --force to repair without a backup."
                );
            }
            tracing::warn!("doctor repair: backup failed but --force set, proceeding: {e}");
        }
    }

    for key in keys {
        match crate::maintenance::apply_remediation_key(db, config, key).await {
            Ok(outcome) if outcome.changed => out.applied.push(key.to_string()),
            Ok(_) => out.noop.push(key.to_string()),
            Err(e) => out.failed.push((key.to_string(), format!("{e}"))),
        }
    }
    Ok(out)
}

/// Create a full `BORG_HOME` backup tarball under `backups/`, returning its
/// path. Reuses [`crate::backup::create_backup`] + the live schema version.
async fn take_backup() -> Result<PathBuf> {
    let home = Config::data_dir()?;
    let out_dir = home.join("backups");
    std::fs::create_dir_all(&out_dir)?;
    let schema_version = crate::backup::live_schema_version().await?;
    crate::backup::create_backup(&home, &out_dir, schema_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::DiagnosticCheck;

    fn report(checks: Vec<DiagnosticCheck>) -> DiagnosticReport {
        DiagnosticReport { checks }
    }

    #[test]
    fn autosafe_keys_collects_and_dedups_only_autosafe() {
        let r =
            report(vec![
                // AutoSafe — should be collected.
                DiagnosticCheck::warn("Data", "data directory exists", "missing").with_remediation(
                    RemediationKind::AutoSafe,
                    "data.ensure_subdirs",
                    "hint",
                ),
                // Duplicate key — must dedup.
                DiagnosticCheck::warn("Embeddings", "whole-entry coverage", "lag")
                    .with_remediation(RemediationKind::AutoSafe, "embeddings.repair", "hint"),
                DiagnosticCheck::warn("Embeddings", "chunk coverage", "lag").with_remediation(
                    RemediationKind::AutoSafe,
                    "embeddings.repair",
                    "hint",
                ),
                // Manual — must be excluded.
                DiagnosticCheck::warn("Provider", "Model retirement", "soon").with_remediation(
                    RemediationKind::Manual,
                    "provider.model_retirement",
                    "hint",
                ),
                // Plain pass — no remediation, excluded.
                DiagnosticCheck::pass("Sandbox", "sandbox-exec available"),
            ]);
        assert_eq!(
            autosafe_keys(&r),
            vec!["data.ensure_subdirs", "embeddings.repair"],
            "only distinct AutoSafe keys, in first-seen order",
        );
    }

    #[test]
    fn autosafe_keys_empty_when_no_autosafe() {
        let r = report(vec![
            DiagnosticCheck::pass("Settings", "config values valid"),
            DiagnosticCheck::warn("Provider", "Model retirement", "soon").with_remediation(
                RemediationKind::Manual,
                "provider.model_retirement",
                "hint",
            ),
        ]);
        assert!(autosafe_keys(&r).is_empty());
    }

    #[tokio::test]
    async fn repair_is_noop_and_takes_no_backup_when_nothing_autosafe() {
        // A report with only Manual findings must short-circuit before the
        // backup step — no recovery point is written for a no-op repair.
        let db = Database::test_db().await;
        let config = Config::default();
        let r = report(vec![DiagnosticCheck::warn(
            "Provider",
            "Model retirement",
            "soon",
        )
        .with_remediation(
            RemediationKind::Manual,
            "provider.model_retirement",
            "hint",
        )]);

        let out = repair_from_report(&db, &config, &r, false).await.unwrap();
        assert!(out.nothing_to_do(), "no AutoSafe issues → nothing applied");
        assert!(
            out.backup_path.is_none(),
            "must not take a backup when there is nothing to repair",
        );
    }
}
