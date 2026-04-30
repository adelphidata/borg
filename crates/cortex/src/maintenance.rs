//! Daily self-healing maintenance task.
//!
//! Runs as a seeded scheduled task with `task_type = 'maintenance'` — the
//! dispatcher invokes `run_daily_maintenance` directly instead of the
//! LLM agent loop. The goal is routine self-care that the user would
//! otherwise have to do by hand: prune old logs, verify health, surface
//! persistent warnings.
//!
//! Failures in individual steps are logged and skipped; a single broken
//! step never stops the rest of the sweep from running. The returned
//! `MaintenanceReport` is persisted to the `doctor_runs` table so the
//! TUI / CLI can render history.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::db::Database;
use crate::doctor::{run_diagnostics, CheckStatus, DiagnosticCheck, RemediationKind};

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
    #[serde(default)]
    pub wedged_runs_recovered: usize,
    /// `messages` rows deleted by the retention purge. `0` when
    /// `data.retention_days` is `None` or `data.purge_policy != Auto`.
    #[serde(default)]
    pub messages_purged: usize,
    /// `audit_log_chain` rows tombstoned by the retention purge.
    /// Tombstones preserve hash-chain linkage; the row count is the
    /// observable side of the daily retention behavior.
    #[serde(default)]
    pub audit_events_tombstoned: usize,
    /// Path to a weekly auto-backup tarball, when one was created on
    /// this run. `None` outside the weekly cadence or when
    /// `data.backup_keep_count == 0`.
    #[serde(default)]
    pub auto_backup_path: Option<String>,
    /// Older `borg-backup-*.tar.zst` files removed by retention pruning.
    #[serde(default)]
    pub backups_pruned: usize,
    /// Doctor checks persisted across runs (subset of `current_issues`
    /// whose issue-state rows are still open with `consecutive_count >= 2`.
    pub persistent_warnings: Vec<String>,
    /// Every Warn/Fail check this run, keyed `"Category:Name"`. Used as
    /// the structured input to persistent-warning detection on the next
    /// sweep — do not format for display, the text lives in `check_summary`.
    #[serde(default)]
    pub current_issues: Vec<String>,
    /// Issues auto-fixed during this sweep.
    #[serde(default)]
    pub auto_fixed_issues: Vec<String>,
    /// Issues still open after all safe repairs ran.
    #[serde(default)]
    pub unhealed_issues: Vec<String>,
    /// Number of non-pass issues marked detect-only this sweep.
    #[serde(default)]
    pub detect_only_issues: usize,
    /// Number of non-pass issues marked manual-only this sweep.
    #[serde(default)]
    pub manual_only_issues: usize,
    /// Number of non-pass issues whose auto-fix attempt failed this sweep.
    #[serde(default)]
    pub auto_fix_failed_issues: usize,
    /// Number of memory entries whose embedding state was repaired.
    #[serde(default)]
    pub embeddings_repaired_files: usize,
    /// Number of chunk rows rebuilt during embedding repair.
    #[serde(default)]
    pub embeddings_repaired_chunks: usize,
    /// Raw check lines (formatted) for display.
    pub check_summary: Vec<String>,
    /// True when maintenance was skipped because `maintenance.enabled`
    /// is false. The report still exists so callers can surface an
    /// explicit "skipped" status instead of presenting a no-op run as
    /// a successful sweep.
    #[serde(default)]
    pub skipped: bool,
}

impl MaintenanceReport {
    /// Short one-line summary suitable for the activity log.
    pub fn activity_line(&self) -> String {
        if self.skipped {
            return "maintenance: skipped (maintenance.enabled = false)".to_string();
        }
        format!(
            "maintenance: {} pass / {} warn / {} fail — pruned {} log(s) ({} bytes truncated), {} activity row(s), {} cache embedding(s), {} workflow(s), healed {} stalled task(s), recovered {} wedged run(s), repaired {} embedding file(s)/{} chunk(s); retention: {} message(s) purged, {} audit event(s) tombstoned",
            self.pass_count,
            self.warn_count,
            self.fail_count,
            self.log_files_deleted,
            self.log_bytes_truncated,
            self.activity_rows_deleted,
            self.embeddings_pruned,
            self.workflows_pruned,
            self.stalled_tasks_healed,
            self.wedged_runs_recovered,
            self.embeddings_repaired_files,
            self.embeddings_repaired_chunks,
            self.messages_purged,
            self.audit_events_tombstoned,
        )
    }
}

#[derive(Debug, Clone)]
struct IssueSnapshot {
    issue_key: String,
    status: &'static str,
    remediation_kind: RemediationKind,
    remediation_key: Option<&'static str>,
}

#[derive(Debug, Default)]
struct EmbeddingRepairReport {
    repaired_files: usize,
    repaired_chunks: usize,
    failed: bool,
    fixed_issue_keys: Vec<String>,
}

#[derive(Debug, Default)]
struct RepairExecution {
    fixed_issues: BTreeSet<String>,
    failed_issues: BTreeSet<String>,
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
        report.skipped = true;
        tracing::info!("maintenance: disabled by config, skipping sweep");
        return Ok(report);
    }

    // 0. Self-heal fresh-install state. These are idempotent and cheap;
    //    running them every sweep makes the daemon's first start order
    //    irrelevant and lets the user delete subdirs without breaking
    //    later checks. Failures log+continue.
    if let Ok(data_dir) = Config::data_dir() {
        if let Err(e) = ensure_data_subdirs(&data_dir) {
            tracing::warn!("maintenance: ensure_data_subdirs failed: {e}");
        }
    }
    if let Err(e) = ensure_global_index(db) {
        tracing::warn!("maintenance: ensure_global_index failed: {e}");
    }

    // 1. Headless doctor sweep, before bounded auto-repairs.
    let initial_diag = run_diagnostics(config);
    let initial_issues = issue_snapshots(&initial_diag.checks);

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

    // 4c. Apply data retention. Deletes `messages` rows past the cutoff
    //     and tombstones `audit_log_chain` rows in place,
    //     preserving hash-chain linkage. Skipped when `retention_days`
    //     is unset or `purge_policy` is `Never` / `OnDemandOnly` — see
    //     `DataConfig::should_auto_purge`.
    if config.data.should_auto_purge() {
        if let Some(cutoff) = config.data.purge_cutoff(now) {
            match db.purge_messages_before(cutoff) {
                Ok(n) => report.messages_purged = n,
                Err(e) => tracing::warn!("maintenance: messages retention purge failed: {e}"),
            }
            match db.tombstone_audit_events_before(cutoff, now) {
                Ok(n) => report.audit_events_tombstoned = n,
                Err(e) => tracing::warn!("maintenance: audit tombstoning failed: {e}"),
            }
        }
    }

    // 4d. Weekly auto-backup. Fires on Sunday when no backup
    //     exists in the last 7 days and `backup_keep_count > 0`. Pruning
    //     happens after a successful create so a failed backup doesn't
    //     erase the last good copy.
    if let Ok(home) = Config::data_dir() {
        let backups_dir = home.join("backups");
        let keep = config.data.backup_keep_count as usize;
        if keep > 0 && weekly_backup_due(&backups_dir, now) {
            let schema_version = db.schema_version().unwrap_or(0);
            match crate::backup::create_backup(&home, &backups_dir, schema_version) {
                Ok(path) => {
                    report.auto_backup_path = Some(path.display().to_string());
                    match crate::backup::prune_backups(&backups_dir, keep) {
                        Ok(n) => report.backups_pruned = n,
                        Err(e) => tracing::warn!("maintenance: backup prune failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("maintenance: weekly backup create failed: {e}"),
            }
        } else if keep == 0 {
            // User explicitly disabled auto-backup — remove any stragglers.
            match crate::backup::prune_backups(&backups_dir, 0) {
                Ok(n) if n > 0 => report.backups_pruned = n,
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    "maintenance: prune of disabled-auto-backup stragglers failed: {e}"
                ),
            }
        }
    }

    // 5. Proactively scan for stalled scheduled tasks. This duplicates
    //    what the daemon loop does on a 5-minute cadence, but running it
    //    here covers the case where the daemon isn't running and the user
    //    fires this task manually with `borg schedule run <id>`.
    match crate::tasks::heal_stalled_tasks(db, now, crate::constants::STALLED_TASK_GRACE_SECS) {
        Ok(h) => report.stalled_tasks_healed = h.reset,
        Err(e) => tracing::warn!("maintenance: stalled-task scan failed: {e}"),
    }
    match crate::tasks::recover_wedged_runs(db, now, crate::constants::STALLED_TASK_GRACE_SECS) {
        Ok(n) => report.wedged_runs_recovered = n as usize,
        Err(e) => tracing::warn!("maintenance: wedged-run sweep failed: {e}"),
    }

    // 6. Run bounded auto-remediations exactly once per remediation key.
    let mut repair_execution = RepairExecution::default();
    let grouped = group_issues_by_remediation(&initial_issues);
    for (key, issues) in grouped {
        let result = match key {
            "data.ensure_subdirs" => Config::data_dir()
                .and_then(|dir| ensure_data_subdirs(&dir))
                .map(|_| (true, None)),
            "memory.ensure_global_index" => ensure_global_index(db)
                .map(|changed| (changed, None)),
            "embeddings.repair" => repair_embedding_backlog(db, config)
                .map(|r| {
                    report.embeddings_repaired_files += r.repaired_files;
                    report.embeddings_repaired_chunks += r.repaired_chunks;
                    (r.repaired_files > 0 || r.repaired_chunks > 0 || !r.failed, Some(r))
                }),
            _ => Ok((false, None)),
        };
        match result {
            Ok((changed, maybe_embedding_report)) => {
                if let Some(embedding_report) = maybe_embedding_report {
                    for issue in embedding_report.fixed_issue_keys {
                        repair_execution.fixed_issues.insert(issue);
                    }
                    if embedding_report.failed {
                        for issue in &issues {
                            repair_execution.failed_issues.insert(issue.issue_key.clone());
                        }
                    }
                } else if changed {
                    for issue in &issues {
                        repair_execution.fixed_issues.insert(issue.issue_key.clone());
                    }
                }
                for issue in &issues {
                    if let Err(e) = db.upsert_issue_state_observed(&issue.issue_key, now, issue.status)
                    {
                        tracing::warn!("maintenance: issue-state observe failed: {e}");
                    }
                    let outcome = if repair_execution.failed_issues.contains(&issue.issue_key) {
                        "failed"
                    } else {
                        "attempted"
                    };
                    if let Err(e) = db.record_issue_auto_fix_attempt(&issue.issue_key, now, outcome) {
                        tracing::warn!("maintenance: issue auto-fix attempt record failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("maintenance: remediation {key} failed: {e}");
                for issue in &issues {
                    repair_execution.failed_issues.insert(issue.issue_key.clone());
                    if let Err(se) = db.upsert_issue_state_observed(&issue.issue_key, now, issue.status)
                    {
                        tracing::warn!("maintenance: issue-state observe failed: {se}");
                    }
                    if let Err(se) = db.record_issue_auto_fix_attempt(&issue.issue_key, now, "failed") {
                        tracing::warn!("maintenance: issue auto-fix attempt record failed: {se}");
                    }
                }
            }
        }
    }

    // 7. Re-run diagnostics so the recorded report reflects post-repair state.
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
    let final_issues = issue_snapshots(&diag.checks);
    report.current_issues = final_issues.iter().map(|i| i.issue_key.clone()).collect();
    report.unhealed_issues = report.current_issues.clone();
    report.auto_fixed_issues = repair_execution.fixed_issues.into_iter().collect();
    report.auto_fix_failed_issues = repair_execution.failed_issues.len();
    report.detect_only_issues = final_issues
        .iter()
        .filter(|i| i.remediation_kind == RemediationKind::None)
        .count();
    report.manual_only_issues = final_issues
        .iter()
        .filter(|i| i.remediation_kind == RemediationKind::Manual)
        .count();

    for issue in &initial_issues {
        if let Err(e) = db.upsert_issue_state_observed(&issue.issue_key, now, issue.status) {
            tracing::warn!("maintenance: issue-state observe failed: {e}");
        }
    }
    let final_keys: BTreeSet<String> = final_issues.iter().map(|i| i.issue_key.clone()).collect();
    for issue in &final_issues {
        if let Err(e) = db.upsert_issue_state_observed(&issue.issue_key, now, issue.status) {
            tracing::warn!("maintenance: issue-state final observe failed: {e}");
        }
    }
    let initial_keys: BTreeSet<String> = initial_issues.iter().map(|i| i.issue_key.clone()).collect();
    for resolved in initial_keys.difference(&final_keys) {
        if let Err(e) = db.resolve_issue_state(resolved, now) {
            tracing::warn!("maintenance: issue-state resolve failed: {e}");
        }
    }
    let resolved_cutoff = now - (config.maintenance.issue_state_keep_days as i64) * SECS_PER_DAY;
    if let Err(e) = db.prune_resolved_issue_state_before(resolved_cutoff) {
        tracing::warn!("maintenance: issue-state prune failed: {e}");
    }
    report.persistent_warnings = db
        .list_persistent_open_issues()
        .map(|rows| rows.into_iter().map(|r| r.issue_key).collect())
        .unwrap_or_default();

    // 8. Persist the run (best-effort; losing the audit row must not
    //    block the sweep).
    if let Err(e) = db.record_doctor_run(&report) {
        tracing::warn!("maintenance: failed to record doctor_runs row: {e}");
    } else if let Err(e) = db.prune_doctor_runs(config.maintenance.doctor_runs_keep as usize) {
        tracing::warn!("maintenance: failed to prune doctor_runs: {e}");
    }

    tracing::info!("{}", report.activity_line());
    Ok(report)
}

/// True when a weekly auto-backup is due. The cadence is "no backup in
/// the last 7 days" — simpler than gating on day-of-week, and survives
/// daemon downtime (a daemon offline for two weeks creates one backup
/// when it next starts, not zero).
fn weekly_backup_due(dir: &Path, now: i64) -> bool {
    if !dir.exists() {
        return true;
    }
    let mut newest_ts: i64 = 0;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return true,
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_prefix("borg-backup-")
            .and_then(|s| s.strip_suffix(".tar.zst"))
        else {
            continue;
        };
        if let Ok(ts) = stem.parse::<i64>() {
            if ts > newest_ts {
                newest_ts = ts;
            }
        }
    }
    const SEVEN_DAYS_SECS: i64 = 7 * 86_400;
    now - newest_ts >= SEVEN_DAYS_SECS
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
    f.seek(SeekFrom::Start(0))?;
    f.write_all(trimmed)?;
    f.set_len(trimmed.len() as u64)?;
    f.sync_all()?;
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

/// Subdirectories of `~/.borg/` that the runtime expects to exist. They are
/// created lazily by their owners (channels on first script-channel install,
/// backups on first backup, etc.); seeding them here means a fresh install
/// passes every doctor check on boot instead of warning until something has
/// happened to populate them.
///
/// `secrets/` is intentionally NOT in this list. The identity layer
/// (`crates/identity/src/keychain.rs`) creates it with mode `0o700` on
/// first KEK write; pre-creating it here with `create_dir_all` would leave
/// it `0o755 & ~umask` — world/group readable — exposing the encrypted
/// KEK file's parent on multi-user hosts. `FileKekStore` owns that path.
const ENSURED_SUBDIRS: &[&str] = &["channels", "skills", "hooks", "backups", "logs"];

/// Idempotently create the runtime subdirectories under `data_dir`. Public
/// so the daemon can call this on startup as well as the daily sweep —
/// either path produces the same end state.
pub fn ensure_data_subdirs(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    for sub in ENSURED_SUBDIRS {
        let path = data_dir.join(sub);
        if let Err(e) = fs::create_dir_all(&path) {
            // Don't abort the sweep over one missing subdir — log and try
            // the rest.
            tracing::warn!("maintenance: could not create {path:?}: {e}");
        }
    }
    Ok(())
}

/// Seed `global/INDEX` if other `global` entries exist but no INDEX. The
/// nightly memory consolidation crons will rewrite the body later — this
/// just makes sure the doctor "no INDEX entry" check stops firing on a
/// fresh install. Returns `true` if a new INDEX was written.
pub fn ensure_global_index(db: &Database) -> Result<bool> {
    let entries = db.list_memory_entries("global")?;
    if entries.is_empty() {
        // No memory at all yet — doctor's check_memory passes ("no entries
        // in DB yet"), so nothing to seed.
        return Ok(false);
    }
    if entries.iter().any(|e| e.name == "INDEX") {
        return Ok(false);
    }
    let mut body = String::from("# Memory Index\n\n");
    for e in &entries {
        body.push_str(&format!("- {}\n", e.name));
    }
    db.upsert_memory_entry("global", "INDEX", &body)?;
    tracing::info!(
        "maintenance: seeded global/INDEX with {} entry/entries",
        entries.len()
    );
    Ok(true)
}

fn issue_snapshots(checks: &[DiagnosticCheck]) -> Vec<IssueSnapshot> {
    checks
        .iter()
        .filter_map(|check| match &check.status {
            CheckStatus::Pass => None,
            CheckStatus::Warn(_) => Some(IssueSnapshot {
                issue_key: check.issue_key(),
                status: "warn",
                remediation_kind: check.remediation_kind,
                remediation_key: check.remediation_key,
            }),
            CheckStatus::Fail(_) => Some(IssueSnapshot {
                issue_key: check.issue_key(),
                status: "fail",
                remediation_kind: check.remediation_kind,
                remediation_key: check.remediation_key,
            }),
        })
        .collect()
}

fn group_issues_by_remediation<'a>(
    issues: &'a [IssueSnapshot],
) -> BTreeMap<&'static str, Vec<&'a IssueSnapshot>> {
    let mut out: BTreeMap<&'static str, Vec<&IssueSnapshot>> = BTreeMap::new();
    for issue in issues {
        if issue.remediation_kind != RemediationKind::AutoSafe {
            continue;
        }
        if let Some(key) = issue.remediation_key {
            out.entry(key).or_default().push(issue);
        }
    }
    out
}

fn embedding_scopes(db: &Database, config: &Config) -> Vec<String> {
    let mut scopes = crate::memory::resolved_memory_scopes(config);
    if let Ok(mut db_scopes) = db.list_memory_scopes() {
        scopes.append(&mut db_scopes);
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn count_stale_chunks(
    expected_chunks: &[crate::chunker::Chunk],
    existing_chunks: &[crate::db::ChunkRow],
) -> usize {
    if expected_chunks.is_empty() {
        return 0;
    }
    expected_chunks
        .iter()
        .enumerate()
        .filter(|(idx, expected)| {
            existing_chunks
                .get(*idx)
                .is_none_or(|existing| {
                    existing.content_hash != crate::embeddings::content_hash(&expected.content)
                        || existing.embedding.is_none()
                })
        })
        .count()
}

fn repair_embedding_backlog(db: &Database, config: &Config) -> Result<EmbeddingRepairReport> {
    let mut report = EmbeddingRepairReport::default();
    if !config.maintenance.embedding_repair_enabled || !config.memory.embeddings.enabled {
        return Ok(report);
    }
    if crate::embeddings::get_or_init_provider(&config.memory.embeddings).is_none() {
        report.failed = true;
        return Ok(report);
    }

    let max_files = config.maintenance.embedding_repair_max_files as usize;
    let max_chunks = config.maintenance.embedding_repair_max_chunks as usize;
    let scopes = embedding_scopes(db, config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    for scope in scopes {
        let entries = db.list_memory_entries(&scope)?;
        for entry in entries {
            if report.repaired_files >= max_files {
                return Ok(report);
            }
            let whole_missing = db
                .get_embedding(&scope, &entry.name)?
                .is_none_or(|row| row.content_hash != entry.content_hash);
            let expected_chunks = crate::chunker::chunk_content(
                &entry.content,
                config.memory.embeddings.chunk_size_tokens,
                config.memory.embeddings.chunk_overlap_tokens,
            );
            let existing_chunks = db.get_chunks_for_file(&scope, &entry.name)?;
            let stale_chunk_count = count_stale_chunks(&expected_chunks, &existing_chunks);
            let chunk_missing = expected_chunks.is_empty()
                || existing_chunks.len() != expected_chunks.len()
                || stale_chunk_count > 0;
            if !whole_missing && !chunk_missing {
                continue;
            }
            if chunk_missing {
                let needed_chunks = stale_chunk_count;
                if report.repaired_chunks + needed_chunks > max_chunks {
                    continue;
                }
            }

            let scope_clone = scope.clone();
            let name_clone = entry.name.clone();
            let content_clone = entry.content.clone();
            let config_clone = config.clone();
            runtime.block_on(async {
                if whole_missing {
                    crate::embeddings::embed_memory_file(
                        &config_clone,
                        &name_clone,
                        &content_clone,
                        &scope_clone,
                    )
                    .await?;
                }
                if chunk_missing {
                    crate::embeddings::embed_memory_file_chunked(
                        &config_clone,
                        &name_clone,
                        &content_clone,
                        &scope_clone,
                    )
                    .await?;
                }
                Ok::<(), anyhow::Error>(())
            })?;

            report.repaired_files += 1;
            if chunk_missing {
                report.repaired_chunks += stale_chunk_count;
                report
                    .fixed_issue_keys
                    .push("Embeddings:chunk coverage".to_string());
            }
            if whole_missing {
                report
                    .fixed_issue_keys
                    .push("Embeddings:whole-entry coverage".to_string());
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ensure_data_subdirs_creates_missing_dirs_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        ensure_data_subdirs(root).expect("first call");
        for sub in ENSURED_SUBDIRS {
            assert!(root.join(sub).is_dir(), "{sub} should be created");
        }
        // Pre-populate one subdir with content, then re-run: must not delete or error.
        let marker = root.join("channels").join("preexisting");
        fs::create_dir_all(&marker).unwrap();
        ensure_data_subdirs(root).expect("second call should not error");
        assert!(marker.is_dir(), "second call should not blow away contents");
    }

    #[test]
    fn ensure_global_index_seeds_when_entries_exist_without_index() {
        let db = Database::test_db();
        db.upsert_memory_entry("global", "notes", "stuff").unwrap();
        let seeded = ensure_global_index(&db).unwrap();
        assert!(seeded, "should write a fresh INDEX");
        let idx = db
            .get_memory_entry("global", "INDEX")
            .unwrap()
            .expect("INDEX must exist");
        assert!(idx.content.contains("- notes"));
    }

    #[test]
    fn ensure_global_index_noop_when_no_entries() {
        let db = Database::test_db();
        let seeded = ensure_global_index(&db).unwrap();
        assert!(!seeded, "no entries means nothing to seed");
        assert!(
            db.get_memory_entry("global", "INDEX").unwrap().is_none(),
            "must not write an INDEX into an empty scope"
        );
    }

    #[test]
    fn ensure_global_index_noop_when_index_already_exists() {
        let db = Database::test_db();
        db.upsert_memory_entry("global", "INDEX", "user-curated body")
            .unwrap();
        db.upsert_memory_entry("global", "notes", "x").unwrap();
        let seeded = ensure_global_index(&db).unwrap();
        assert!(!seeded, "must not overwrite an existing INDEX");
        let idx = db.get_memory_entry("global", "INDEX").unwrap().unwrap();
        assert_eq!(
            idx.content, "user-curated body",
            "existing INDEX content must be preserved verbatim"
        );
    }

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
            log_bytes_truncated: 4_321,
            workflows_pruned: 2,
            activity_rows_deleted: 120,
            embeddings_pruned: 8,
            stalled_tasks_healed: 1,
            wedged_runs_recovered: 3,
            ..Default::default()
        };
        let line = r.activity_line();
        assert!(line.contains("14 pass"));
        assert!(line.contains("2 warn"));
        assert!(line.contains("pruned 3 log"));
        assert!(line.contains("4321 bytes truncated"));
        assert!(line.contains("2 workflow"));
        assert!(line.contains("healed 1 stalled"));
        assert!(line.contains("recovered 3 wedged"));
    }

    #[test]
    fn report_activity_line_mentions_skip() {
        let line = MaintenanceReport {
            skipped: true,
            ..Default::default()
        }
        .activity_line();
        assert_eq!(line, "maintenance: skipped (maintenance.enabled = false)");
    }

    /// Writes a file with predictable line content and returns `(path, size)`.
    /// Each line is `"line N\n"` — predictable length makes assertions easy.
    fn write_sample_log(dir: &Path, name: &str, approx_bytes: usize) -> (std::path::PathBuf, u64) {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        let mut written = 0usize;
        let mut n = 0u64;
        while written < approx_bytes {
            let line = format!("line {n}\n");
            f.write_all(line.as_bytes()).unwrap();
            written += line.len();
            n += 1;
        }
        f.sync_all().unwrap();
        let size = fs::metadata(&path).unwrap().len();
        (path, size)
    }

    #[test]
    fn truncate_oversized_logs_head_truncates_and_starts_at_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // 8 MB file, cap 5 MB, keep 1 MB.
        let (path, original) = write_sample_log(dir, "daemon.log", 8 * 1024 * 1024);
        assert!(original > 5 * 1024 * 1024);

        let freed =
            truncate_oversized_logs(dir, &["daemon.log"], 5 * 1024 * 1024, 1024 * 1024).unwrap();

        let new_size = fs::metadata(&path).unwrap().len();
        // Must be close to keep_bytes and strictly smaller than original.
        assert!(new_size < original, "file should shrink");
        assert!(
            new_size <= 1024 * 1024,
            "new size {new_size} must not exceed keep_bytes"
        );
        // Freed count must reflect bytes actually removed.
        assert_eq!(freed, original - new_size);

        // The retained head must be a complete line — "line " prefix, not
        // a mid-line fragment. This is the invariant that guarantees future
        // log parsers don't choke on a half-truncated first row.
        let head = fs::read_to_string(&path).unwrap();
        assert!(
            head.starts_with("line "),
            "retained head must start at a line boundary, got: {:?}",
            &head[..head.len().min(40)]
        );
    }

    #[test]
    fn truncate_oversized_logs_noop_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (path, original) = write_sample_log(dir, "tui.log", 2 * 1024 * 1024);

        let freed =
            truncate_oversized_logs(dir, &["tui.log"], 5 * 1024 * 1024, 1024 * 1024).unwrap();

        assert_eq!(freed, 0);
        let new_size = fs::metadata(&path).unwrap().len();
        assert_eq!(new_size, original, "file under cap must not change");
    }

    #[test]
    fn truncate_oversized_logs_preserves_live_append_handles() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("daemon.log");
        let mut writer = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for n in 0..200_000 {
            writeln!(writer, "line {n}").unwrap();
        }
        writer.flush().unwrap();

        let freed = truncate_oversized_logs(dir, &["daemon.log"], 1024 * 1024, 256 * 1024).unwrap();
        assert!(freed > 0, "oversized log should have been truncated");

        writeln!(writer, "tail after truncation").unwrap();
        writer.flush().unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("tail after truncation"),
            "writes through the original append handle must still land in the visible log file"
        );
    }

    #[test]
    fn truncate_oversized_logs_tolerates_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Nothing written; list references files that don't exist.
        let freed = truncate_oversized_logs(
            dir,
            &["daemon.log", "tui.log", "daemon.err"],
            5 * 1024 * 1024,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(freed, 0);
    }

    #[test]
    fn group_issues_by_remediation_deduplicates_auto_safe_keys() {
        let issues = vec![
            IssueSnapshot {
                issue_key: "Embeddings:chunk coverage".into(),
                status: "warn",
                remediation_kind: RemediationKind::AutoSafe,
                remediation_key: Some("embeddings.repair"),
            },
            IssueSnapshot {
                issue_key: "Embeddings:whole-entry coverage".into(),
                status: "warn",
                remediation_kind: RemediationKind::AutoSafe,
                remediation_key: Some("embeddings.repair"),
            },
            IssueSnapshot {
                issue_key: "Gateway:channel manifest".into(),
                status: "fail",
                remediation_kind: RemediationKind::Manual,
                remediation_key: None,
            },
        ];
        let grouped = group_issues_by_remediation(&issues);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped["embeddings.repair"].len(), 2);
    }

    /// Integration test: feeding a real `Config` with `data.retention_days
    /// = Some(1)` through `run_daily_maintenance` must purge old messages
    /// and tombstone old audit-chain rows, leaving the chain verifiable.
    /// Failure mode: the wiring between `DataConfig::should_auto_purge` and
    /// the maintenance step regressing silently — the daemon would happily
    /// run the sweep but never purge anything.
    #[test]
    fn run_daily_maintenance_executes_retention_when_configured() {
        use crate::config::{Config, DataConfig, PurgePolicy};
        use crate::db::AuditChainOutcome;

        let db = crate::db::Database::test_db();
        // Two old audit events (well past cutoff) + one young one.
        let now = chrono::Utc::now().timestamp();
        let old_ts = now - 100 * SECS_PER_DAY;
        let young_ts = now - 1; // safely under any reasonable retention cutoff
        db.append_audit_event("evt", r#"{"i":1}"#, "p", old_ts)
            .unwrap();
        db.append_audit_event("evt", r#"{"i":2}"#, "p", old_ts + 1)
            .unwrap();
        db.append_audit_event("evt", r#"{"i":3}"#, "p", young_ts)
            .unwrap();
        // Two old messages, one young.
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'user', 'old1', ?1),
                        ('s', 'user', 'old2', ?2),
                        ('s', 'user', 'young', ?3)",
                rusqlite::params![old_ts, old_ts + 1, young_ts],
            )
            .unwrap();

        let mut config = Config::default();
        config.data = DataConfig {
            retention_days: Some(7),
            purge_policy: PurgePolicy::Auto,
            backup_keep_count: 0,
        };

        let report = run_daily_maintenance(&db, &config).expect("sweep");
        assert_eq!(report.messages_purged, 2);
        assert_eq!(report.audit_events_tombstoned, 2);

        // The young row stayed; the chain still verifies with two
        // tombstones in place.
        let remaining_msgs: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_msgs, 1);
        match db.verify_audit_chain().expect("verify") {
            AuditChainOutcome::OkWithTombstones { count } => assert_eq!(count, 2),
            other => panic!("expected OkWithTombstones(2), got {other:?}"),
        }
    }

    /// Weekly cadence: no backup → due. A backup written 1 hour ago →
    /// not due. A backup written 8 days ago → due again. Failure mode:
    /// using day-of-week instead of "last_ts > 7d ago" would miss the
    /// case of a daemon offline for >7 days starting on a non-Sunday.
    #[test]
    fn weekly_backup_due_uses_seven_day_window() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = 1_900_000_000;
        // Empty dir → due.
        assert!(weekly_backup_due(dir, now));
        // Recent backup → not due.
        std::fs::write(
            dir.join(format!("borg-backup-{}.tar.zst", now - 3600)),
            b"x",
        )
        .unwrap();
        assert!(!weekly_backup_due(dir, now));
        // Old backup (8 days) plus the recent one → still not due (we
        // pick the newest).
        std::fs::write(
            dir.join(format!("borg-backup-{}.tar.zst", now - 9 * 86400)),
            b"x",
        )
        .unwrap();
        assert!(!weekly_backup_due(dir, now));
        // Remove recent → due again.
        std::fs::remove_file(dir.join(format!("borg-backup-{}.tar.zst", now - 3600))).unwrap();
        assert!(weekly_backup_due(dir, now));
    }

    /// `purge_policy = Never` skips retention even when `retention_days`
    /// is set — the litigation-hold escape hatch from `data-handling.md`.
    #[test]
    fn run_daily_maintenance_skips_retention_when_policy_never() {
        use crate::config::{Config, DataConfig, PurgePolicy};

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        let old_ts = now - 100 * SECS_PER_DAY;
        db.append_audit_event("evt", r#"{"i":1}"#, "p", old_ts)
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s', 'user', 'old', ?1)",
                rusqlite::params![old_ts],
            )
            .unwrap();

        let mut config = Config::default();
        config.data = DataConfig {
            retention_days: Some(7),
            purge_policy: PurgePolicy::Never,
            backup_keep_count: 0,
        };

        let report = run_daily_maintenance(&db, &config).expect("sweep");
        assert_eq!(report.messages_purged, 0);
        assert_eq!(report.audit_events_tombstoned, 0);

        let remaining_msgs: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_msgs, 1);
    }

    #[test]
    fn run_daily_maintenance_recovers_wedged_runs() {
        use crate::db::NewTask;
        use crate::tasks::RUN_STATUS_RUNNING;
        use rusqlite::params;

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        db.create_task(&NewTask {
            id: "wedged-task",
            name: "wedged",
            prompt: "p",
            schedule_type: "cron",
            schedule_expr: "0 0 * * * *",
            timezone: "local",
            next_run: None,
            max_retries: None,
            timeout_ms: Some(60_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
        })
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
                 VALUES (?1, ?2, 0, ?3)",
                params!["wedged-task", now - 3600, RUN_STATUS_RUNNING],
            )
            .unwrap();

        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert_eq!(report.wedged_runs_recovered, 1);

        let status: String = db
            .conn()
            .query_row(
                "SELECT status FROM task_runs WHERE task_id = 'wedged-task' ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, crate::tasks::RUN_STATUS_FAILED);
    }

    #[test]
    fn run_daily_maintenance_reports_skip_without_side_effects() {
        use crate::config::Config;

        let db = crate::db::Database::test_db();
        let mut config = Config::default();
        config.maintenance.enabled = false;

        let report = run_daily_maintenance(&db, &config).expect("sweep");
        assert!(report.skipped, "disabled maintenance should surface a skip");
        assert_eq!(report.log_files_deleted, 0);
        assert_eq!(report.activity_rows_deleted, 0);

        let doctor_runs: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM doctor_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            doctor_runs, 0,
            "skipped sweep must not record doctor history"
        );
    }

    #[test]
    fn run_daily_maintenance_populates_issue_state_persistence() {
        let db = crate::db::Database::test_db();
        let mut config = Config::default();
        config.memory.embeddings.enabled = false;
        let first = run_daily_maintenance(&db, &config).expect("first");
        let second = run_daily_maintenance(&db, &config).expect("second");
        assert!(
            second
                .persistent_warnings
                .iter()
                .any(|w| w == "Embeddings:semantic memory search"),
            "repeated unresolved issue should become persistent"
        );
        assert!(
            first.persistent_warnings.is_empty(),
            "single-run warnings should not be persistent"
        );
    }

    #[test]
    fn repair_embedding_backlog_no_provider_is_bounded_failure() {
        let db = crate::db::Database::test_db();
        let mut config = Config::default();
        config.memory.embeddings.enabled = true;
        config.memory.embeddings.provider = Some("openai".into());
        config.memory.embeddings.api_key_env = Some("BORG_TEST_MISSING_EMBED_KEY".into());
        std::env::remove_var("BORG_TEST_MISSING_EMBED_KEY");

        let report = repair_embedding_backlog(&db, &config).expect("repair");
        assert!(report.failed, "missing provider creds should skip repair");
        assert_eq!(report.repaired_files, 0);
        assert_eq!(report.repaired_chunks, 0);
    }

    #[test]
    fn count_stale_chunks_only_counts_missing_or_stale_entries() {
        let expected_chunks = vec![
            crate::chunker::Chunk {
                content: "alpha".into(),
                start_line: 1,
                end_line: 1,
            },
            crate::chunker::Chunk {
                content: "beta".into(),
                start_line: 2,
                end_line: 2,
            },
            crate::chunker::Chunk {
                content: "gamma".into(),
                start_line: 3,
                end_line: 3,
            },
        ];
        let existing_chunks = vec![
            crate::db::ChunkRow {
                id: 1,
                scope: "global".into(),
                filename: "doc".into(),
                chunk_index: 0,
                start_line: Some(1),
                end_line: Some(1),
                content: "alpha".into(),
                content_hash: crate::embeddings::content_hash("alpha"),
                embedding: Some(vec![1, 2, 3]),
                created_at: 0,
            },
            crate::db::ChunkRow {
                id: 2,
                scope: "global".into(),
                filename: "doc".into(),
                chunk_index: 1,
                start_line: Some(2),
                end_line: Some(2),
                content: "beta".into(),
                content_hash: crate::embeddings::content_hash("beta"),
                embedding: None,
                created_at: 0,
            },
        ];

        assert_eq!(
            count_stale_chunks(&expected_chunks, &existing_chunks),
            2,
            "only the missing tail chunk and the stale middle chunk should count against the budget"
        );
    }
}
