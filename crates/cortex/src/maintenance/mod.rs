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
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::constants::SECS_PER_DAY;
use crate::db::Database;
use crate::doctor::{run_diagnostics_with_registry, DiagnosticCheck, RemediationKind};
use crate::process_registry::{compile_never_kill_patterns, AutoReapPolicy, ProcessRegistry};

mod budget;
mod embeddings;
mod fs_setup;
mod gateway_recovery;
mod issues;
mod log_prune;

pub use fs_setup::{ensure_data_subdirs, ensure_global_index};
pub use gateway_recovery::recover_gateway_if_abandoned;

use budget::BudgetTracker;
use embeddings::{repair_embedding_backlog, EmbeddingRepairReport, EMBEDDING_CACHE_TTL_SECS};
use issues::{group_issues_by_remediation, issue_snapshots, RepairExecution};
use log_prune::{
    prune_log_files, truncate_oversized_logs, CAPPED_LOG_FILES, LOG_FILE_KEEP_BYTES,
    LOG_FILE_SIZE_CAP_BYTES,
};

pub use crate::task_ids::MAINTENANCE_TASK_ID;

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
    /// Doctor checks persisted across runs (subset of `current_issues`
    /// whose issue-state rows are still open with `consecutive_count >= 2`.
    pub persistent_warnings: Vec<String>,
    /// `/doctor` squelches whose TTL elapsed and were removed this sweep.
    /// The corresponding warnings will resume on the next heartbeat /
    /// doctor run.
    #[serde(default)]
    pub squelches_expired: usize,
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
    /// Pages reclaimed by the WAL checkpoint that runs every sweep.
    /// `-1` when the checkpoint failed; any non-negative value is a
    /// real reclamation count from `PRAGMA wal_checkpoint(TRUNCATE)`.
    #[serde(default)]
    pub wal_checkpoint_pages_reclaimed: i64,
    /// True when the conditional full `VACUUM` ran this sweep.
    #[serde(default)]
    pub vacuum_ran: bool,
    /// Bytes the DB file shrank during the full VACUUM. `0` when VACUUM
    /// didn't run or when the file size didn't change.
    #[serde(default)]
    pub vacuum_bytes_reclaimed: i64,
    /// True when this sweep stopped early because
    /// [`MaintenanceConfig::heal_action_budget`] was exhausted. Existing
    /// step counters reflect what *did* run; later steps short-circuit.
    #[serde(default)]
    pub budget_exhausted: bool,
    /// Number of mutating actions counted against the heal action
    /// budget this sweep. Useful telemetry for tuning the default
    /// budget (1000) — most healthy sweeps stay well below.
    #[serde(default)]
    pub heal_actions_spent: u32,
    /// Subprocess registry entries whose PID vanished (daemon crash
    /// mid-spawn or kernel-reaped before the guard fired) — moved from
    /// running to finished as `Dead`.
    #[serde(default)]
    pub subprocess_dead_reaped: usize,
    /// Subprocess registry finished-queue entries evicted by TTL/LRU.
    #[serde(default)]
    pub subprocess_finished_pruned: usize,
    /// Non-pinned running entries past the warn/fail thresholds. Drives
    /// the `Subprocesses:Long-running children` doctor check.
    #[serde(default)]
    pub subprocess_long_running: usize,
    /// Entries SIGTERM'd / SIGKILL'd by `auto_reap` this sweep.
    #[serde(default)]
    pub subprocess_auto_killed: usize,
    /// Entries past the auto-reap age threshold but spared by an
    /// exemption (kind, pattern, or pin).
    #[serde(default)]
    pub subprocess_auto_exempted: usize,
}

impl MaintenanceReport {
    /// True when the sweep did or observed something worth surfacing at
    /// INFO. A no-op sweep (everything zero, no warnings, no vacuum)
    /// stays at DEBUG so production logs aren't dominated by hourly
    /// "nothing to do" lines.
    pub fn is_noteworthy(&self) -> bool {
        if self.skipped {
            return false;
        }
        self.warn_count > 0
            || self.fail_count > 0
            || self.log_files_deleted > 0
            || self.activity_rows_deleted > 0
            || self.embeddings_pruned > 0
            || self.stalled_tasks_healed > 0
            || self.wedged_runs_recovered > 0
            || self.embeddings_repaired_files > 0
            || self.messages_purged > 0
            || self.audit_events_tombstoned > 0
            || self.vacuum_ran
            || self.budget_exhausted
            || !self.persistent_warnings.is_empty()
            || !self.unhealed_issues.is_empty()
            || self.auto_fix_failed_issues > 0
            || self.squelches_expired > 0
            || self.wal_checkpoint_pages_reclaimed < 0
            || self.subprocess_dead_reaped > 0
            || self.subprocess_auto_killed > 0
    }

    /// Short one-line summary suitable for the activity log.
    pub fn activity_line(&self) -> String {
        if self.skipped {
            return "maintenance: skipped (maintenance.enabled = false)".to_string();
        }
        format!(
            "maintenance: {} pass / {} warn / {} fail — pruned {} log(s) ({} bytes truncated), {} activity row(s), {} cache embedding(s), healed {} stalled task(s), recovered {} wedged run(s), repaired {} embedding file(s)/{} chunk(s); retention: {} message(s) purged, {} audit event(s) tombstoned",
            self.pass_count,
            self.warn_count,
            self.fail_count,
            self.log_files_deleted,
            self.log_bytes_truncated,
            self.activity_rows_deleted,
            self.embeddings_pruned,
            self.stalled_tasks_healed,
            self.wedged_runs_recovered,
            self.embeddings_repaired_files,
            self.embeddings_repaired_chunks,
            self.messages_purged,
            self.audit_events_tombstoned,
        )
    }
}

/// Run the full daily maintenance sweep without a process registry —
/// preserved for call sites (CLI standalone runs, tests) that don't
/// have access to the daemon's live registry. Equivalent to
/// [`run_daily_maintenance_with_registry`] with `None`.
///
/// The daemon's task dispatcher and `Admin.Heal` RPC call the registry
/// variant so subprocess reap/auto-kill actually runs in production.
pub async fn run_daily_maintenance(
    db: &Database,
    config: &Config,
) -> Result<MaintenanceReport> {
    run_daily_maintenance_with_registry(db, config, None).await
}

/// Run the full daily maintenance sweep. Never propagates errors from
/// individual steps — each is logged and the sweep continues so one
/// broken step can't block the others.
pub async fn run_daily_maintenance_with_registry(
    db: &Database,
    config: &Config,
    registry: Option<&ProcessRegistry>,
) -> Result<MaintenanceReport> {
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

    let mut budget = BudgetTracker::new(config.maintenance.heal_action_budget);

    // 0. Self-heal fresh-install state. These are idempotent and cheap;
    //    running them every sweep makes the daemon's first start order
    //    irrelevant and lets the user delete subdirs without breaking
    //    later checks. Failures log+continue.
    if let Ok(data_dir) = Config::data_dir() {
        if let Err(e) = ensure_data_subdirs(&data_dir) {
            tracing::warn!("maintenance: ensure_data_subdirs failed: {e}");
        }
    }
    if let Err(e) = ensure_global_index(db).await {
        tracing::warn!("maintenance: ensure_global_index failed: {e}");
    }

    // 1. Headless doctor sweep, before bounded auto-repairs.
    let initial_diag = run_diagnostics_with_registry(config, registry).await;
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
    if !budget.is_blown() {
        match db.prune_activity_before(activity_cutoff).await {
            Ok(n) => {
                report.activity_rows_deleted = n;
                budget.try_spend(n);
            }
            Err(e) => tracing::warn!("maintenance: activity pruning failed: {e}"),
        }
    }

    // 4. Prune stale embedding-cache entries.
    if !budget.is_blown() {
        match db.prune_embedding_cache(EMBEDDING_CACHE_TTL_SECS).await {
            Ok(n) => {
                report.embeddings_pruned = n;
                budget.try_spend(n);
            }
            Err(e) => tracing::warn!("maintenance: embedding prune failed: {e}"),
        }
    }

    // 4a. Expire `/doctor` squelches whose TTL has passed. Once pruned,
    //     the corresponding warning re-surfaces in subsequent heartbeats —
    //     security findings the user has acknowledged are intentionally
    //     bounded so they don't disappear from the radar forever.
    if !budget.is_blown() {
        match db.doctor_squelch_prune_expired(now).await {
            Ok(n) => {
                report.squelches_expired = n;
                budget.try_spend(n);
            }
            Err(e) => tracing::warn!("maintenance: doctor-squelch prune failed: {e}"),
        }
    }

    // 4b. Apply data retention. Deletes `messages` rows past the cutoff
    //     and tombstones `audit_log_chain` rows in place,
    //     preserving hash-chain linkage. Skipped when `retention_days`
    //     is unset or `purge_policy` is `Never` / `OnDemandOnly` — see
    //     `DataConfig::should_auto_purge`.
    if !budget.is_blown() && config.data.should_auto_purge() {
        if let Some(cutoff) = config.data.purge_cutoff(now) {
            match db.purge_messages_before(cutoff).await {
                Ok(n) => {
                    report.messages_purged = n;
                    budget.try_spend(n);
                }
                Err(e) => tracing::warn!("maintenance: messages retention purge failed: {e}"),
            }
            if !budget.is_blown() {
                match db.tombstone_audit_events_before(cutoff, now).await {
                    Ok(n) => {
                        report.audit_events_tombstoned = n;
                        budget.try_spend(n);
                    }
                    Err(e) => tracing::warn!("maintenance: audit tombstoning failed: {e}"),
                }
            }
        }
    }

    // 5. Proactively scan for stalled scheduled tasks. This duplicates
    //    what the daemon loop does on a 5-minute cadence, but running it
    //    here covers the case where the daemon isn't running and the user
    //    fires this task manually with `borg schedule run <id>`.
    if !budget.is_blown() {
        match crate::tasks::heal_stalled_tasks(db, now, crate::constants::STALLED_TASK_GRACE_SECS)
            .await
        {
            Ok(h) => {
                report.stalled_tasks_healed = h.reset;
                budget.try_spend(h.reset);
            }
            Err(e) => tracing::warn!("maintenance: stalled-task scan failed: {e}"),
        }
    }
    if !budget.is_blown() {
        match crate::tasks::recover_wedged_runs(db, now, crate::constants::STALLED_TASK_GRACE_SECS)
            .await
        {
            Ok(n) => {
                report.wedged_runs_recovered = n as usize;
                budget.try_spend(n as usize);
            }
            Err(e) => tracing::warn!("maintenance: wedged-run sweep failed: {e}"),
        }
    }

    // 5_subproc. Subprocess registry sweep. Three sub-steps:
    //   1. reap_dead — kill -0 probe on every Running PID; vanished
    //      ones (daemon crashed mid-spawn, kernel reaped before guard
    //      fired) move to finished as Dead.
    //   2. auto_reap — SIGTERM/SIGKILL any non-exempt entry past the
    //      `process_registry.auto_reap_secs` threshold (default 24h).
    //      Kind=Browser, pinned entries, and pattern-matched commands
    //      are exempt. The activity log captures the killed command
    //      for post-mortem before the entry is moved.
    //   3. prune_finished — TTL+LRU eviction of completed entries.
    //
    // No-op when the caller didn't pass a registry (CLI standalone /
    // tests). Each sub-action charges the heal_action_budget.
    let mut subprocess_repeat_offender = false;
    if let Some(reg) = registry {
        if !budget.is_blown() {
            let reaped = reg.reap_dead(now);
            report.subprocess_dead_reaped = reaped;
            budget.try_spend(reaped);
        }
        if !budget.is_blown() && config.process_registry.auto_reap_secs > 0 {
            let policy = AutoReapPolicy {
                max_age_secs: config.process_registry.auto_reap_secs as i64,
                never_kill_patterns: compile_never_kill_patterns(
                    &config.process_registry.never_kill_patterns,
                ),
                sigterm_grace_secs: config.process_registry.sigterm_grace_secs,
            };
            // Previous-sweep snapshot must be read before auto_reap
            // overwrites it.
            let previous = reg.previous_auto_reaped_commands();
            let auto_report = reg.auto_reap(now, &policy);
            report.subprocess_auto_killed = auto_report.killed_count();
            report.subprocess_auto_exempted = auto_report.exempted_count();
            budget.try_spend(auto_report.killed_count());

            // Audit-log each kill with the full command so the operator
            // can post-mortem. Loud-on-the-error-path: never silently
            // discard the activity-log write.
            for entry in auto_report
                .terminated
                .iter()
                .chain(auto_report.killed.iter())
            {
                let line = format!(
                    "auto-reaped subprocess id={} pid={} kind={} age={}s cmd={:?}",
                    entry.id,
                    entry.pid,
                    entry.kind.tag(),
                    now.saturating_sub(entry.started_at),
                    entry.command
                );
                crate::activity_log::log_activity(db, "warn", "process_registry", &line).await;
            }

            // Repeat-offender: same normalized command auto-reaped two
            // sweeps in a row → surface a persistent warning hinting
            // the operator to add it to `never_kill_patterns`.
            let current = reg.previous_auto_reaped_commands();
            if !current.is_empty() && current.iter().any(|c| previous.contains(c)) {
                subprocess_repeat_offender = true;
            }
        }
        if !budget.is_blown() {
            let pruned = reg.prune_finished(
                now,
                config.process_registry.finished_ttl_secs as i64,
                config.process_registry.max_finished_entries,
            );
            report.subprocess_finished_pruned = pruned;
            budget.try_spend(pruned);
        }
        // Classify still-running entries for the doctor surface. Counted
        // even when auto-reap is disabled — visibility is the primary
        // signal; auto-kill is the long-tail backstop.
        let class = reg.classify(
            now,
            config.process_registry.orphan_warn_secs as i64,
            config.process_registry.orphan_fail_secs as i64,
        );
        report.subprocess_long_running = class.warning.len() + class.failing.len();
    }

    // 5a. WAL checkpoint + conditional full VACUUM. Always runs the
    //     cheap checkpoint after the heavy deletes above; gates full
    //     VACUUM behind both an interval and a fragmentation signal.
    //     Not counted against the action budget — these are SQLite
    //     housekeeping, not row-mutating work, and the bounded VACUUM
    //     gating is already a separate budget.
    let wv =
        crate::db::wal::run_wal_and_vacuum(db, now, config.maintenance.vacuum_interval_days).await;
    report.wal_checkpoint_pages_reclaimed = wv.wal_checkpoint_pages_reclaimed;
    report.vacuum_ran = wv.vacuum_ran;
    report.vacuum_bytes_reclaimed = wv.vacuum_bytes_reclaimed;

    // 5b. Gateway recovery probe. The orchestrator's respawn supervisor
    //     gives up after `GATEWAY_MAX_CRASH_RESPAWNS` and writes
    //     `gateway_unrecoverable=1` to the meta table. The heartbeat
    //     `gateway_unrecoverable` augmenter then nudges the user — but
    //     that nudge is the *backup* path; this step is the
    //     self-healing path that runs first.
    //
    //     Recovery action here is intentionally narrow: probe the
    //     configured listen address. If something now answers — daemon
    //     was restarted, port was freed, an upstream crash cleared —
    //     clear the flag so subsequent heartbeats stop nagging. We
    //     don't try to re-spawn the supervisor task from sync code
    //     (that requires runtime/orchestrator handles we don't have
    //     here); a clean daemon restart is still required for true
    //     recovery, but this stops the alert from being a permanent
    //     scar after the user already fixed the underlying issue.
    if let Err(e) = recover_gateway_if_abandoned(db, config).await {
        tracing::warn!("maintenance: gateway recovery probe failed: {e}");
    }

    // 6. Run bounded auto-remediations exactly once per remediation key.
    //    Budget-blown sweeps skip remediation entirely — the budget is the
    //    "something is genuinely runaway" backstop, and re-running repairs
    //    on a stuck row is exactly the failure mode it exists to prevent.
    let mut repair_execution = RepairExecution::default();
    let grouped = if budget.is_blown() {
        BTreeMap::new()
    } else {
        group_issues_by_remediation(&initial_issues)
    };
    for (key, issues) in grouped {
        let result: Result<(bool, Option<EmbeddingRepairReport>)> = match key {
            "data.ensure_subdirs" => Config::data_dir()
                .and_then(|dir| ensure_data_subdirs(&dir))
                .map(|_| (true, None)),
            "memory.ensure_global_index" => ensure_global_index(db)
                .await
                .map(|changed| (changed, None)),
            "embeddings.repair" => repair_embedding_backlog(db, config).await.map(|r| {
                report.embeddings_repaired_files += r.repaired_files;
                report.embeddings_repaired_chunks += r.repaired_chunks;
                budget.try_spend(r.repaired_files);
                (
                    r.repaired_files > 0 || r.repaired_chunks > 0 || !r.failed,
                    Some(r),
                )
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
                            repair_execution
                                .failed_issues
                                .insert(issue.issue_key.clone());
                        }
                    }
                } else if changed {
                    for issue in &issues {
                        repair_execution
                            .fixed_issues
                            .insert(issue.issue_key.clone());
                    }
                }
                for issue in &issues {
                    if let Err(e) = db
                        .upsert_issue_state_observed(&issue.issue_key, now, issue.status)
                        .await
                    {
                        tracing::warn!("maintenance: issue-state observe failed: {e}");
                    }
                    let outcome = if repair_execution.failed_issues.contains(&issue.issue_key) {
                        "failed"
                    } else {
                        "attempted"
                    };
                    if let Err(e) = db
                        .record_issue_auto_fix_attempt(&issue.issue_key, now, outcome)
                        .await
                    {
                        tracing::warn!("maintenance: issue auto-fix attempt record failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("maintenance: remediation {key} failed: {e}");
                for issue in &issues {
                    repair_execution
                        .failed_issues
                        .insert(issue.issue_key.clone());
                    if let Err(se) = db
                        .upsert_issue_state_observed(&issue.issue_key, now, issue.status)
                        .await
                    {
                        tracing::warn!("maintenance: issue-state observe failed: {se}");
                    }
                    if let Err(se) = db
                        .record_issue_auto_fix_attempt(&issue.issue_key, now, "failed")
                        .await
                    {
                        tracing::warn!("maintenance: issue auto-fix attempt record failed: {se}");
                    }
                }
            }
        }
    }

    // 7. Re-run diagnostics so the recorded report reflects post-repair state.
    let mut diag = run_diagnostics_with_registry(config, registry).await;
    if subprocess_repeat_offender {
        // Two consecutive sweeps auto-reaped the same workload. Surface
        // as a persistent warning so the operator either pins it or adds
        // a `never_kill_patterns` entry instead of silently re-killing
        // every 24h.
        diag.checks.push(
            DiagnosticCheck::warn(
                "Subprocesses",
                "repeated auto-reap",
                "the same workload was auto-reaped on two consecutive sweeps",
            )
            .with_remediation(
                RemediationKind::Manual,
                "subprocesses.repeated_auto_reap",
                "add the command to `process_registry.never_kill_patterns` or pin it",
            ),
        );
    }
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
        if let Err(e) = db
            .upsert_issue_state_observed(&issue.issue_key, now, issue.status)
            .await
        {
            tracing::warn!("maintenance: issue-state observe failed: {e}");
        }
    }
    let final_keys: BTreeSet<String> = final_issues.iter().map(|i| i.issue_key.clone()).collect();
    for issue in &final_issues {
        if let Err(e) = db
            .upsert_issue_state_observed(&issue.issue_key, now, issue.status)
            .await
        {
            tracing::warn!("maintenance: issue-state final observe failed: {e}");
        }
    }
    let initial_keys: BTreeSet<String> =
        initial_issues.iter().map(|i| i.issue_key.clone()).collect();
    for resolved in initial_keys.difference(&final_keys) {
        if let Err(e) = db.resolve_issue_state(resolved, now).await {
            tracing::warn!("maintenance: issue-state resolve failed: {e}");
        }
    }
    let resolved_cutoff = now - (config.maintenance.issue_state_keep_days as i64) * SECS_PER_DAY;
    if let Err(e) = db.prune_resolved_issue_state_before(resolved_cutoff).await {
        tracing::warn!("maintenance: issue-state prune failed: {e}");
    }
    // Active /doctor squelches drop their findings out of persistent_warnings
    // — that's the user's "I see this, stop nagging" signal. The squelch
    // expires per `maintenance.security_squelch_ttl_days`, after which the
    // warning resumes.
    let active_squelches: std::collections::HashSet<String> = db
        .doctor_squelch_list_active(now)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| format!("{}:{}", r.category, r.name))
                .collect()
        })
        .unwrap_or_default();
    report.persistent_warnings = db
        .list_persistent_open_issues()
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| r.issue_key)
                .filter(|k| !active_squelches.contains(k))
                .collect()
        })
        .unwrap_or_default();

    // Capture final budget state. `budget_exhausted` is observable to
    // callers and surfaces on the next sweep as a persistent warning
    // when it fires twice in a row (existing maintenance_issue_state
    // plumbing handles the escalation).
    report.budget_exhausted = budget.is_blown();
    report.heal_actions_spent = budget.spent;

    if budget.is_blown() {
        tracing::warn!(
            "maintenance: heal action budget exhausted at {} actions (cap {}); remaining mutating steps skipped",
            budget.spent,
            budget.budget,
        );
        if let Err(e) = db
            .log_activity(
                "warn",
                "maintenance",
                &format!(
                    "heal action budget exhausted at {} actions (cap {}); remaining mutating steps skipped",
                    budget.spent, budget.budget
                ),
                None,
            )
            .await
        {
            tracing::warn!("maintenance: failed to record budget-exhaust activity row: {e}");
        }
        // Surface as an issue so the existing persistent-warning lifecycle
        // promotes it on the second consecutive exhaust.
        if let Err(e) = db
            .upsert_issue_state_observed("Maintenance:heal action budget", now, "warn")
            .await
        {
            tracing::warn!("maintenance: budget issue-state observe failed: {e}");
        }
        // Re-read persistent_warnings now that the budget issue may have
        // been promoted; otherwise the report misses its own escalation
        // for this sweep.
        let active_squelches: std::collections::HashSet<String> = db
            .doctor_squelch_list_active(now)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|r| format!("{}:{}", r.category, r.name))
                    .collect()
            })
            .unwrap_or_default();
        report.persistent_warnings = db
            .list_persistent_open_issues()
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|r| r.issue_key)
                    .filter(|k| !active_squelches.contains(k))
                    .collect()
            })
            .unwrap_or_default();
    }

    // 8. Persist the run (best-effort; losing the audit row must not
    //    block the sweep).
    if let Err(e) = db.record_doctor_run(&report).await {
        tracing::warn!("maintenance: failed to record doctor_runs row: {e}");
    } else if let Err(e) = db
        .prune_doctor_runs(config.maintenance.doctor_runs_keep as usize)
        .await
    {
        tracing::warn!("maintenance: failed to prune doctor_runs: {e}");
    }

    tracing::info!("{}", report.activity_line());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_activity_line_is_informative() {
        let r = MaintenanceReport {
            pass_count: 14,
            warn_count: 2,
            fail_count: 0,
            log_files_deleted: 3,
            log_bytes_truncated: 4_321,
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
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
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
    fn run_daily_maintenance_runs_wal_checkpoint_every_sweep() {
        // SQLite's `journal_mode=WAL` and `wal_checkpoint` only apply to
        // file-backed databases — `:memory:` silently falls back to
        // `memory` journal mode, where `wal_checkpoint` returns an error.
        // The maintenance sweep handles that gracefully (-1 sentinel,
        // sweep continues), but the *real* invariant we're guarding —
        // that production sweeps actually run a checkpoint — needs a
        // file-backed DB to verify.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("borg.db");
        let db = crate::db::Database::open_at(&db_path).expect("open file-backed db");

        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert!(
            report.wal_checkpoint_pages_reclaimed >= 0,
            "file-backed sweep must run a real checkpoint and report a non-negative page count, got {}",
            report.wal_checkpoint_pages_reclaimed
        );
    }

    #[test]
    fn run_daily_maintenance_handles_wal_checkpoint_failure_gracefully() {
        // Failure-mode coverage for the in-memory case: the sweep must
        // still complete, surface the -1 sentinel, and not poison any
        // other field. This is the regression guard for "what happens
        // when the WAL pragma can't run."
        let db = crate::db::Database::test_db();
        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert_eq!(
            report.wal_checkpoint_pages_reclaimed, -1,
            "in-memory DB cannot checkpoint; sentinel must be -1"
        );
        // Other fields populated normally.
        assert!(!report.skipped);
    }

    #[test]
    fn run_daily_maintenance_runs_vacuum_when_fragmented_and_due() {
        use crate::db::wal::META_LAST_VACUUM_AT;

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        // Force interval-elapsed: pretend the last VACUUM was 30 days ago.
        db.set_meta(META_LAST_VACUUM_AT, &(now - 30 * SECS_PER_DAY).to_string())
            .unwrap();
        // Inflate then delete to push freelist > 15% of pages.
        db.conn()
            .execute_batch(
                "CREATE TABLE bloat (id INTEGER, payload BLOB);
                 INSERT INTO bloat (id, payload)
                 SELECT n, randomblob(2048)
                 FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5000)
                       SELECT n FROM c);
                 DELETE FROM bloat WHERE id > 100;",
            )
            .unwrap();

        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert!(
            report.vacuum_ran,
            "fragmentation+interval should trigger VACUUM"
        );
        // Persisted timestamp must be updated so the next sweep skips
        // VACUUM until interval elapses again.
        let stored = db.get_meta(META_LAST_VACUUM_AT).unwrap();
        let parsed: i64 = stored.unwrap().parse().unwrap();
        assert!(
            parsed >= now,
            "last_vacuum_at must be advanced after a successful VACUUM (got {parsed}, expected >= {now})"
        );
    }

    #[test]
    fn run_daily_maintenance_skips_vacuum_when_recent() {
        use crate::db::wal::META_LAST_VACUUM_AT;

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        // Last VACUUM yesterday; default interval 7d → skip.
        db.set_meta(META_LAST_VACUUM_AT, &(now - SECS_PER_DAY).to_string())
            .unwrap();
        db.conn()
            .execute_batch(
                "CREATE TABLE bloat (id INTEGER, payload BLOB);
                 INSERT INTO bloat (id, payload)
                 SELECT n, randomblob(2048)
                 FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5000)
                       SELECT n FROM c);
                 DELETE FROM bloat WHERE id > 100;",
            )
            .unwrap();

        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert!(!report.vacuum_ran, "interval guard should block VACUUM");
        // Stored timestamp must be unchanged — skipped VACUUM is a no-op.
        let stored: i64 = db
            .get_meta(META_LAST_VACUUM_AT)
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(stored, now - SECS_PER_DAY);
    }

    #[test]
    fn run_daily_maintenance_short_circuits_when_budget_exhausted() {
        use crate::db::NewTask;
        use crate::tasks::RUN_STATUS_RUNNING;
        use rusqlite::params;

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        // 50 stale activity rows (will hit step 3, the first mutating step).
        for i in 0..50 {
            db.conn()
                .execute(
                    "INSERT INTO activity_log (level, category, message, detail, created_at)
                     VALUES ('info', 'test', ?1, NULL, ?2)",
                    params![format!("noise {i}"), now - 100 * SECS_PER_DAY],
                )
                .unwrap();
        }
        // 50 wedged task_runs (would hit step 5, *after* the budget blew).
        db.create_task(&NewTask {
            id: "wedged-budget",
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
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .unwrap();
        for _ in 0..50 {
            db.conn()
                .execute(
                    "INSERT INTO task_runs (task_id, started_at, duration_ms, status)
                     VALUES (?1, ?2, 0, ?3)",
                    params!["wedged-budget", now - 3600, RUN_STATUS_RUNNING],
                )
                .unwrap();
        }

        let mut config = Config::default();
        config.maintenance.heal_action_budget = 10;

        let report = run_daily_maintenance(&db, &config).expect("sweep");

        assert!(
            report.budget_exhausted,
            "50 prunes vs budget=10 should blow it"
        );
        assert_eq!(
            report.activity_rows_deleted, 50,
            "the active step still completes; the budget gate is checked between steps"
        );
        assert_eq!(
            report.wedged_runs_recovered, 0,
            "later step must short-circuit once budget is blown"
        );
        // Exactly one warn row recording the exhaustion was written.
        let warn_msgs = db
            .query_activity(100, Some("warn"), Some("maintenance"))
            .unwrap();
        let exhaust_rows: Vec<_> = warn_msgs
            .iter()
            .filter(|m| m.message.contains("heal action budget exhausted"))
            .collect();
        assert_eq!(
            exhaust_rows.len(),
            1,
            "exactly one budget-exhaust activity row, got {}",
            exhaust_rows.len()
        );
    }

    #[test]
    fn run_daily_maintenance_budget_warning_persists_after_two_consecutive_exhausts() {
        use rusqlite::params;

        let db = crate::db::Database::test_db();
        let now = chrono::Utc::now().timestamp();
        // Seed enough activity to blow budget=5.
        for i in 0..20 {
            db.conn()
                .execute(
                    "INSERT INTO activity_log (level, category, message, detail, created_at)
                     VALUES ('info', 'test', ?1, NULL, ?2)",
                    params![format!("noise {i}"), now - 100 * SECS_PER_DAY],
                )
                .unwrap();
        }
        let mut config = Config::default();
        config.maintenance.heal_action_budget = 5;

        let first = run_daily_maintenance(&db, &config).expect("first sweep");
        assert!(first.budget_exhausted);
        // First exhaust is informational — issue is observed but not yet
        // persistent. We don't assert it's *absent*, only that the second
        // surfaces it.

        // Re-seed for a second sweep so there's still work to do.
        for i in 0..20 {
            db.conn()
                .execute(
                    "INSERT INTO activity_log (level, category, message, detail, created_at)
                     VALUES ('info', 'test', ?1, NULL, ?2)",
                    params![format!("noise2 {i}"), now - 100 * SECS_PER_DAY],
                )
                .unwrap();
        }
        let second = run_daily_maintenance(&db, &config).expect("second sweep");
        assert!(second.budget_exhausted);
        assert!(
            second
                .persistent_warnings
                .iter()
                .any(|w| w == "Maintenance:heal action budget"),
            "second consecutive exhaust must promote to persistent_warnings, got {:?}",
            second.persistent_warnings
        );
    }

    #[test]
    fn run_daily_maintenance_does_not_blow_budget_on_healthy_sweep() {
        let db = crate::db::Database::test_db();
        let mut config = Config::default();
        // Generous cap, nothing to clean → must not exhaust.
        config.maintenance.heal_action_budget = 10_000;
        let report = run_daily_maintenance(&db, &config).expect("sweep");
        assert!(!report.budget_exhausted);
        assert!(
            report.heal_actions_spent < 10_000,
            "spent {} actions on a clean DB, expected far less",
            report.heal_actions_spent
        );
    }

    // ── Subprocess registry sweep ──

    #[test]
    fn sweep_reports_zero_subprocess_counts_when_registry_empty() {
        // Empty registry → all subprocess counters are 0. No accidental
        // false-positive warnings on a clean install.
        let db = crate::db::Database::test_db();
        let registry = crate::process_registry::ProcessRegistry::new();
        let report = run_daily_maintenance_with_registry(&db, &Config::default(), Some(&registry))
            .expect("sweep");
        assert_eq!(report.subprocess_dead_reaped, 0);
        assert_eq!(report.subprocess_finished_pruned, 0);
        assert_eq!(report.subprocess_long_running, 0);
        assert_eq!(report.subprocess_auto_killed, 0);
    }

    #[test]
    fn sweep_reaps_dead_pid() {
        // Register a PID that's reliably never alive; the sweep must
        // detect it and move it to finished.
        let db = crate::db::Database::test_db();
        let registry = crate::process_registry::ProcessRegistry::new();
        let _g = registry.register(
            0xFFFF_FFFE,
            crate::process_registry::ProcessKind::Hook,
            "ghost",
            None,
        );
        let report = run_daily_maintenance_with_registry(&db, &Config::default(), Some(&registry))
            .expect("sweep");
        assert_eq!(report.subprocess_dead_reaped, 1);
        std::mem::forget(_g);
    }

    #[test]
    fn sweep_skips_auto_reap_when_setting_zero() {
        // Visibility-only mode: ancient entries report as long-running
        // but are not killed. Uses PID 1 (init / launchd — always
        // alive) so the dead-reap step doesn't evict the entry before
        // classify runs.
        let db = crate::db::Database::test_db();
        let registry = crate::process_registry::ProcessRegistry::new();
        let _g = registry.register(
            1,
            crate::process_registry::ProcessKind::RunShell,
            "ancient",
            None,
        );
        let mut config = Config::default();
        config.process_registry.auto_reap_secs = 0;
        config.process_registry.orphan_warn_secs = 0;
        config.process_registry.orphan_fail_secs = 1_000_000;
        let report =
            run_daily_maintenance_with_registry(&db, &config, Some(&registry)).expect("sweep");
        assert_eq!(report.subprocess_auto_killed, 0, "auto-reap disabled");
        assert!(
            report.subprocess_long_running >= 1,
            "still surfaces as long-running (got {})",
            report.subprocess_long_running,
        );
        std::mem::forget(_g);
    }

    #[test]
    fn sweep_classify_excludes_pinned_from_long_running() {
        // The sweep's long-running counter must mirror the doctor
        // check's exclusion logic: pinned entries don't count. Uses
        // PID 1 to survive the dead-reap probe.
        let db = crate::db::Database::test_db();
        let registry = crate::process_registry::ProcessRegistry::new();
        let g = registry.register(
            1,
            crate::process_registry::ProcessKind::RunShell,
            "pinned-build",
            None,
        );
        registry.pin(g.id()).expect("pin");
        let mut config = Config::default();
        config.process_registry.orphan_warn_secs = 0;
        config.process_registry.orphan_fail_secs = 1_000_000;
        let report =
            run_daily_maintenance_with_registry(&db, &config, Some(&registry)).expect("sweep");
        assert_eq!(report.subprocess_long_running, 0, "pinned excluded");
        std::mem::forget(g);
    }

    #[test]
    fn sweep_with_no_registry_is_a_noop_for_subprocess_fields() {
        // The CLI-standalone path passes None; the legacy callsites
        // (heal RPC pre-wire, tests) must still produce a valid report.
        let db = crate::db::Database::test_db();
        let report = run_daily_maintenance(&db, &Config::default()).expect("sweep");
        assert_eq!(report.subprocess_dead_reaped, 0);
        assert_eq!(report.subprocess_auto_killed, 0);
    }
}
