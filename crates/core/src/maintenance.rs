//! Daily self-healing maintenance task.
//!
//! Runs as a seeded scheduled task with `task_type = 'maintenance'` — the
//! dispatcher invokes [`run_daily_maintenance`] directly instead of the
//! LLM agent loop. The goal is routine self-care that the user would
//! otherwise have to do by hand: prune old logs, verify health, surface
//! persistent warnings.
//!
//! Failures in individual steps are logged and skipped; a single broken
//! step never stops the rest of the sweep from running. The returned
//! [`MaintenanceReport`] is persisted to the `doctor_runs` table so the
//! TUI / CLI can render history.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::db::Database;
use crate::doctor::{run_diagnostics, CheckStatus, DiagnosticCheck};

/// Single source of truth for the seeded maintenance task's UUID. The V37
/// migration inserts this id and the dispatcher matches against it.
pub const MAINTENANCE_TASK_ID: &str = "00000000-0000-4000-8000-ada1ca1e0001";

/// Seconds per day. Retention settings are expressed in days.
const SECS_PER_DAY: i64 = 86_400;

/// How long an embedding-cache entry can sit unaccessed before the
/// maintenance sweep evicts it. Mirrors the 30-day default elsewhere.
const EMBEDDING_CACHE_TTL_SECS: i64 = 30 * SECS_PER_DAY;

/// Size threshold above which `daemon.log`, `daemon.err`, and `tui.log` are
/// head-truncated during the daily sweep. Nothing rotates these files
/// otherwise — a single noisy warn loop once blew them up to >40 MB.
const LOG_FILE_SIZE_CAP_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
/// When a log file exceeds [`LOG_FILE_SIZE_CAP_BYTES`], keep this many
/// trailing bytes (the most recent lines) and discard the rest.
const LOG_FILE_KEEP_BYTES: u64 = 1024 * 1024; // 1 MB

/// Log files the maintenance sweep is allowed to head-truncate. These are
/// appended-to freely by the daemon and TUI with no rotation; every other
/// file under `~/.borg/logs/` (e.g. dated `*.jsonl`) is handled separately.
const CAPPED_LOG_FILES: &[&str] = &["daemon.log", "daemon.err", "tui.log"];

/// Summary of one maintenance run, persisted to `doctor_runs` and
/// returned to the task dispatcher for activity-log accounting.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceReport {
    pub ran_at: i64,
    pub pass_count: usize,
    pub warn_count: usize,
    pub fail_count: usize,
    pub log_files_deleted: usize,
    pub log_bytes_truncated: u64,
    pub workflows_pruned: usize,
    pub activity_rows_deleted: usize,
    pub embeddings_pruned: usize,
    pub stalled_tasks_healed: usize,
    /// Doctor checks persisted across runs (subset of `current_issues`
    /// that were also issues in the previous run).
    pub persistent_warnings: Vec<String>,
    /// Every Warn/Fail check this run, keyed `"Category:Name"`. Used as
    /// the structured input to persistent-warning detection on the next
    /// sweep — do not format for display, the text lives in `check_summary`.
    #[serde(default)]
    pub current_issues: Vec<String>,
    /// Raw check lines (formatted) for display.
    pub check_summary: Vec<String>,
}

impl MaintenanceReport {
    /// Short one-line summary suitable for the activity log.
    pub fn activity_line(&self) -> String {
        format!(
            "maintenance: {} pass / {} warn / {} fail — pruned {} log(s), {} activity row(s), {} embedding(s), healed {} stalled task(s)",
            self.pass_count,
            self.warn_count,
            self.fail_count,
            self.log_files_deleted,
            self.activity_rows_deleted,
            self.embeddings_pruned,
            self.stalled_tasks_healed,
        )
    }
}

/// Run the full daily maintenance sweep. Never propagates errors from
/// individual steps — each is logged and the sweep continues so one
/// broken step can't block the others.
pub fn run_daily_maintenance(db: &Database, config: &Config) -> Result<MaintenanceReport> {
    let now = chrono::Utc::now().timestamp();
    let mut report = MaintenanceReport {
        ran_at: now,
        ..Default::default()
    };

    if !config.maintenance.enabled {
        tracing::info!("maintenance: disabled by config, skipping sweep");
        return Ok(report);
    }

    // 1. Headless doctor sweep. Persist results so we can compare across
    //    runs and only escalate warnings that stick.
    let diag = run_diagnostics(config);
    let (pass, warn, fail) = diag.counts();
    report.pass_count = pass;
    report.warn_count = warn;
    report.fail_count = fail;
    report.check_summary = diag
        .checks
        .iter()
        .map(DiagnosticCheck::format_line)
        .collect();

    // 2. Prune filesystem logs. Best-effort — missing dir is fine.
    if let Ok(logs_dir) = Config::logs_dir() {
        let cutoff = now - (config.maintenance.logs_retention_days as i64) * SECS_PER_DAY;
        match prune_log_files(&logs_dir, cutoff) {
            Ok(n) => report.log_files_deleted = n,
            Err(e) => tracing::warn!("maintenance: log pruning failed: {e}"),
        }
        // Cap unbounded append-only logs (daemon.log, tui.log, daemon.err).
        // These are never rotated by the runtime; a noisy warn loop can
        // balloon them to tens of MB in days.
        match truncate_oversized_logs(
            &logs_dir,
            CAPPED_LOG_FILES,
            LOG_FILE_SIZE_CAP_BYTES,
            LOG_FILE_KEEP_BYTES,
        ) {
            Ok(n) => report.log_bytes_truncated = n,
            Err(e) => tracing::warn!("maintenance: log truncation failed: {e}"),
        }
    }

    // 3. Prune activity-log rows beyond retention window.
    let activity_cutoff = now - (config.maintenance.activity_retention_days as i64) * SECS_PER_DAY;
    match db.prune_activity_before(activity_cutoff) {
        Ok(n) => report.activity_rows_deleted = n,
        Err(e) => tracing::warn!("maintenance: activity pruning failed: {e}"),
    }

    // 4. Prune stale embedding-cache entries.
    match db.prune_embedding_cache(EMBEDDING_CACHE_TTL_SECS) {
        Ok(n) => report.embeddings_pruned = n,
        Err(e) => tracing::warn!("maintenance: embedding prune failed: {e}"),
    }

    // 4b. Prune old completed/failed/cancelled workflows. Without this,
    //     experimental "test" workflows accumulate indefinitely (hundreds
    //     seen in the wild) and, if any sit in `running` across a daemon
    //     restart, burst-process all at once and spam the activity log.
    let workflow_cutoff = now - (config.maintenance.workflow_retention_days as i64) * SECS_PER_DAY;
    match db.prune_completed_workflows(workflow_cutoff) {
        Ok(n) => report.workflows_pruned = n,
        Err(e) => tracing::warn!("maintenance: workflow prune failed: {e}"),
    }

    // 5. Proactively scan for stalled scheduled tasks. This duplicates
    //    what the daemon loop does on a 5-minute cadence, but running it
    //    here covers the case where the daemon isn't running and the user
    //    fires this task manually with `borg schedule run <id>`.
    match crate::tasks::heal_stalled_tasks(db, now, crate::constants::STALLED_TASK_GRACE_SECS) {
        Ok(h) => report.stalled_tasks_healed = h.reset,
        Err(e) => tracing::warn!("maintenance: stalled-task scan failed: {e}"),
    }

    // 6. Compare against the previous run — warnings that persist across
    //    two consecutive sweeps are escalated. A single flaky run doesn't
    //    page the user.
    report.current_issues = diag
        .checks
        .iter()
        .filter(|c| !matches!(c.status, CheckStatus::Pass))
        .map(|c| format!("{}:{}", c.category, c.name))
        .collect();
    report.persistent_warnings = compute_persistent_warnings(db, &report.current_issues);

    // 7. Persist the run (best-effort; losing the audit row must not
    //    block the sweep).
    if let Err(e) = db.record_doctor_run(&report) {
        tracing::warn!("maintenance: failed to record doctor_runs row: {e}");
    } else if let Err(e) = db.prune_doctor_runs(config.maintenance.doctor_runs_keep as usize) {
        tracing::warn!("maintenance: failed to prune doctor_runs: {e}");
    }

    tracing::info!("{}", report.activity_line());
    Ok(report)
}

/// Head-truncate append-only log files under `dir` whose size exceeds
/// `cap_bytes`, keeping the last `keep_bytes` of each. Returns the total
/// number of bytes freed across all affected files.
///
/// These logs (`daemon.log`, `tui.log`, `daemon.err`) have no runtime
/// rotation. A single misbehaving warn! loop blew `daemon.log` past 20 MB
/// in under two weeks; this cap prevents recurrence.
fn truncate_oversized_logs(
    dir: &Path,
    names: &[&str],
    cap_bytes: u64,
    keep_bytes: u64,
) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut freed: u64 = 0;
    for name in names {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let size = meta.len();
        if size <= cap_bytes {
            continue;
        }
        match truncate_file_keeping_tail(&path, keep_bytes) {
            Ok(new_size) => {
                let delta = size.saturating_sub(new_size);
                freed = freed.saturating_add(delta);
                tracing::info!(
                    "maintenance: truncated {} ({} MB → {} MB)",
                    name,
                    size / (1024 * 1024),
                    new_size / (1024 * 1024),
                );
            }
            Err(e) => tracing::warn!("maintenance: could not truncate {}: {e}", path.display()),
        }
    }
    Ok(freed)
}

/// Rewrite `path` to contain only its last `keep_bytes`. Best-effort
/// attempts to start at a newline so the new head of the file is a
/// complete log line rather than a mid-line fragment.
fn truncate_file_keeping_tail(path: &Path, keep_bytes: u64) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let total = f.metadata()?.len();
    let start = total.saturating_sub(keep_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(keep_bytes as usize);
    f.read_to_end(&mut tail)?;
    // Advance to the first newline so we don't keep a half-truncated head line.
    let offset = tail
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let trimmed = &tail[offset..];

    // Replace the file contents atomically via a sibling tempfile rename.
    let tmp = path.with_extension("trunc.tmp");
    {
        let mut out = fs::File::create(&tmp)?;
        out.write_all(trimmed)?;
        out.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(trimmed.len() as u64)
}

/// Delete `*.jsonl` log files under `dir` whose mtime is older than
/// `cutoff` (unix seconds). Returns deletion count.
fn prune_log_files(dir: &Path, cutoff: i64) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut deleted = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if (since_epoch.as_secs() as i64) < cutoff {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!("maintenance: could not remove log {path:?}: {e}");
                continue;
            }
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Return the subset of current Warn/Fail issues that were ALSO present
/// in the most recent prior `doctor_runs` row. Transient warnings that
/// only appeared on this run are excluded — the point is to surface
/// nags, not flukes.
///
/// Compares structured `"Category:Name"` keys directly; no text parsing.
fn compute_persistent_warnings(db: &Database, current_issues: &[String]) -> Vec<String> {
    if current_issues.is_empty() {
        return Vec::new();
    }
    let Ok(Some(prior)) = db.latest_doctor_run() else {
        return Vec::new();
    };
    let prior_set: std::collections::HashSet<&str> =
        prior.current_issues.iter().map(String::as_str).collect();
    current_issues
        .iter()
        .filter(|issue| prior_set.contains(issue.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn prune_log_files_deletes_only_jsonl_older_than_cutoff() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();

        let jsonl = dir.join("old.jsonl");
        let other = dir.join("keep.txt");
        fs::write(&jsonl, b"old").unwrap();
        fs::write(&other, b"other").unwrap();

        // Cutoff far in the future: every file looks "old", but only the
        // .jsonl should be deleted.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let far_future_cutoff = now + 365 * SECS_PER_DAY;
        let deleted = prune_log_files(dir, far_future_cutoff).unwrap();
        assert_eq!(deleted, 1, "only the .jsonl should be pruned");
        assert!(!jsonl.exists(), "jsonl should be gone");
        assert!(other.exists(), "non-jsonl file should remain");
    }

    #[test]
    fn prune_log_files_keeps_files_newer_than_cutoff() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();
        let jsonl = dir.join("fresh.jsonl");
        fs::write(&jsonl, b"fresh").unwrap();

        // Cutoff in the past: everything is newer than cutoff, nothing is pruned.
        let deleted = prune_log_files(dir, 0).unwrap();
        assert_eq!(deleted, 0);
        assert!(jsonl.exists());
    }

    #[test]
    fn prune_log_files_handles_missing_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(prune_log_files(&missing, 0).unwrap(), 0);
    }

    #[test]
    fn report_activity_line_is_informative() {
        let r = MaintenanceReport {
            pass_count: 14,
            warn_count: 2,
            fail_count: 0,
            log_files_deleted: 3,
            activity_rows_deleted: 120,
            embeddings_pruned: 8,
            stalled_tasks_healed: 1,
            ..Default::default()
        };
        let line = r.activity_line();
        assert!(line.contains("14 pass"));
        assert!(line.contains("2 warn"));
        assert!(line.contains("pruned 3 log"));
        assert!(line.contains("healed 1 stalled"));
    }
}
