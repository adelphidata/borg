//! Daemon-loop self-healing steps.
//!
//! These are the recovery actions the dispatcher loop in `scheduler.rs`
//! used to inline directly:
//!
//! - **Boot recovery** — at daemon start, mark previously in-flight task
//!   runs as failed (the daemon crashed mid-execution).
//! - **Drift recovery** — after a host wake-from-sleep, advance the
//!   `next_run` of any task more than seven days overdue (otherwise the
//!   dispatcher would replay every missed window in a burst).
//! - **Periodic stalled-task scan** — every five minutes, heal task rows
//!   stuck in `pending` past the grace window and recover task runs that
//!   never finished.
//!
//! All three live here so the scheduler loop body is just `match` arms
//! over claimed work — boot/drift/stall recovery becomes a single typed
//! call into `Maintenance::*`.

use cortex::db::Database;

/// How many seconds overdue a task must be before its `next_run` is
/// auto-advanced rather than fired immediately. Catches the wake-from-sleep
/// case where a laptop returns after weeks and would otherwise replay
/// every missed daily.
pub(crate) const STALE_TASK_THRESHOLD_SECS: i64 = 7 * 86_400;

/// Outcome of one boot-time recovery sweep.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BootRecovery {
    /// Number of `task_runs` rows that were in-flight at the previous
    /// daemon's exit and were just marked failed.
    pub stale_runs_recovered: u64,
    /// Number of due tasks whose `next_run` was auto-advanced because
    /// they were more than [`STALE_TASK_THRESHOLD_SECS`] overdue.
    pub stale_tasks_advanced: usize,
    /// Number of per-session leases freed because the worker that held them
    /// died (lease deadline already passed). Lets any worker reclaim the
    /// session and resume it from the durable log.
    pub stale_leases_expired: u64,
}

/// Outcome of one drift-recovery sweep (post-sleep wake-up).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DriftRecovery {
    pub stale_tasks_advanced: usize,
}

/// Outcome of one periodic stalled-task scan.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StalledScan {
    pub tasks_healed: usize,
    pub wedged_runs_recovered: u64,
}

/// Mark in-flight task runs as failed (daemon crashed mid-execution) and
/// advance any due tasks more than seven days overdue. Returns the
/// observed counts; failures inside individual steps log and yield zero.
///
/// Runs still executing in this process (`cortex::tasks::live_run_ids`) are
/// left alone: the dispatcher is supervised, and a respawn after a wedge does
/// not stop the runs its previous incarnation spawned. The scheduler also
/// only calls this once per process, so a respawn never re-fails rows or
/// re-reclaims the session leases the live process holds.
pub(crate) async fn boot_recovery(db: &Database) -> BootRecovery {
    let stale_runs_recovered = match db
        .recover_stale_runs(
            "Daemon crashed during execution",
            &cortex::tasks::live_run_ids(),
        )
        .await
    {
        Ok(n) => {
            if n > 0 {
                // Real anomaly worth surfacing — daemon crashed mid-run.
                tracing::info!(
                    count = n,
                    "recovered stale task runs from previous daemon crash"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!("recover_stale_runs failed at boot: {e}");
            0
        }
    };
    let stale_tasks_advanced = advance_stale_tasks(db).await;
    // Cold boot: every on-disk lease belongs to a previous (now-dead) daemon
    // instance, so reclaim them all — not just the TTL-expired ones — or a
    // crash-restart within the lease TTL would leave the new process unable to
    // resume its own sessions.
    let stale_leases_expired = match db.reclaim_all_session_leases().await {
        Ok(n) => {
            if n > 0 {
                tracing::info!(
                    count = n,
                    "freed session leases held by a previous daemon instance"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!("reclaim_all_session_leases failed at boot: {e}");
            0
        }
    };
    BootRecovery {
        stale_runs_recovered,
        stale_tasks_advanced,
        stale_leases_expired,
    }
}

/// Re-check for stale tasks after a wall-clock jump (host wake-from-sleep).
/// Same logic as the boot-time pass, minus the `recover_stale_runs` step.
pub(crate) async fn drift_recovery(db: &Database) -> DriftRecovery {
    DriftRecovery {
        stale_tasks_advanced: advance_stale_tasks(db).await,
    }
}

/// Periodic stalled-task scan — runs every `STALLED_TASK_SCAN_INTERVAL`
/// from the dispatcher loop. Heals task rows stuck in `pending` past the
/// grace window and recovers task runs that never finished.
pub(crate) async fn stalled_scan(db: &Database, now: i64, grace_secs: i64) -> StalledScan {
    let tasks_healed = match cortex::tasks::heal_stalled_tasks(db, now, grace_secs).await {
        Ok(report) => report.reset,
        Err(e) => {
            tracing::warn!("Stalled-task scan failed: {e}");
            0
        }
    };
    let wedged_runs_recovered = match cortex::tasks::recover_wedged_runs(db, now, grace_secs).await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("Wedged-run sweep failed: {e}");
            0
        }
    };
    StalledScan {
        tasks_healed,
        wedged_runs_recovered,
    }
}

async fn advance_stale_tasks(db: &Database) -> usize {
    let now = chrono::Utc::now().timestamp();
    let stale_tasks = match db.get_due_tasks(now).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("get_due_tasks failed during stale-task advance: {e}");
            return 0;
        }
    };
    let mut advanced = 0;
    for task in &stale_tasks {
        let age = now - task.next_run.unwrap_or(0);
        if age <= STALE_TASK_THRESHOLD_SECS {
            continue;
        }
        // One-shot tasks are not what this suppression is for. Advancing a
        // recurring task skips the occurrences missed while the daemon was
        // down, which is the intent; advancing a one-shot marks it
        // `completed` with `next_run = NULL`, so a task the user explicitly
        // asked for is closed as done having never run once. Let it fire
        // late instead — late is what the user wants from a reminder they
        // are only now getting a machine back for.
        if task.schedule_type == cortex::tasks::SCHEDULE_TYPE_ONCE {
            tracing::info!(
                task = %task.name,
                overdue_days = age / 86_400,
                "one-shot task is overdue; running it late rather than closing it unrun",
            );
            continue;
        }
        tracing::warn!(
            "Skipping stale task '{}' ({}d overdue)",
            task.name,
            age / 86_400
        );
        if let Err(e) = cortex::tasks::advance_next_run(task, db).await {
            tracing::warn!(
                error = %e,
                task = %task.name,
                "advance_next_run on stale task failed",
            );
        } else {
            advanced += 1;
        }
    }
    advanced
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::db::{Database, NewTask};

    async fn test_db() -> Database {
        Database::test_db().await
    }

    async fn seed_interval_task(db: &Database, id: &str, next_run: Option<i64>) {
        db.create_task(&NewTask {
            id,
            name: id,
            prompt: "noop",
            schedule_type: "interval",
            schedule_expr: "1m",
            timezone: "UTC",
            next_run,
            max_retries: Some(0),
            timeout_ms: Some(60_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("create_task");
    }

    async fn next_run_of(db: &Database, id: &str) -> Option<i64> {
        db.get_task_by_id(id)
            .await
            .expect("read task")
            .expect("task exists")
            .next_run
    }

    #[tokio::test]
    async fn boot_recovery_marks_stale_runs_failed() {
        let db = test_db().await;
        seed_interval_task(&db, "t1", Some(0)).await;
        // Manufacture an in-flight run row.
        let _run_id = db.start_task_run("t1", 100).await.expect("start run");

        let report = boot_recovery(&db).await;
        assert!(
            report.stale_runs_recovered >= 1,
            "expected at least one stale run, got {report:?}"
        );

        // Run row must now be marked failed.
        let runs = db.task_run_history("t1", 5).await.expect("history");
        let stale_run = runs.first().expect("at least one run");
        assert_eq!(
            stale_run.status, "failed",
            "stale run must be marked failed: {stale_run:?}"
        );
    }

    /// A supervisor respawn of the dispatcher must not fail the runs the
    /// previous incarnation spawned: they are still polling in this process
    /// and will write their own final status.
    #[tokio::test]
    async fn boot_recovery_skips_runs_that_are_live_in_this_process() {
        let db = test_db().await;
        seed_interval_task(&db, "live", Some(0)).await;
        seed_interval_task(&db, "dead", Some(0)).await;
        let live_id = db.start_task_run("live", 100).await.expect("start run");
        let _dead_id = db.start_task_run("dead", 100).await.expect("start run");
        let _guard = cortex::tasks::LiveRunGuard::register(live_id);

        let report = boot_recovery(&db).await;
        assert_eq!(
            report.stale_runs_recovered, 1,
            "only the dead run: {report:?}"
        );

        let live_run = db
            .task_run_history("live", 1)
            .await
            .expect("history")
            .remove(0);
        assert_eq!(live_run.status, "running", "live run must be left running");
        assert_eq!(live_run.error, None, "live run's error must be unchanged");
        let dead_run = db
            .task_run_history("dead", 1)
            .await
            .expect("history")
            .remove(0);
        assert_eq!(
            dead_run.status, "failed",
            "a non-live running row is still failed"
        );
    }

    #[tokio::test]
    async fn boot_recovery_advances_overdue_tasks() {
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        seed_interval_task(&db, "stale", Some(now - 8 * 86_400)).await;
        seed_interval_task(&db, "fresh", Some(now - 60)).await;

        let report = boot_recovery(&db).await;
        assert_eq!(
            report.stale_tasks_advanced, 1,
            "exactly one stale task, got {report:?}"
        );
        assert!(
            next_run_of(&db, "stale").await.unwrap() > now,
            "stale advanced"
        );
        assert_eq!(
            next_run_of(&db, "fresh").await,
            Some(now - 60),
            "fresh task must not be touched"
        );
    }

    #[tokio::test]
    async fn drift_recovery_advances_only_stale_tasks() {
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        seed_interval_task(&db, "stale", Some(now - 10 * 86_400)).await;
        seed_interval_task(&db, "boundary", Some(now - 7 * 86_400)).await;

        let report = drift_recovery(&db).await;
        // Boundary at exactly 7 days overdue must NOT advance — predicate is
        // `age > 7 * 86_400`. Catches a future `>=` regression.
        assert_eq!(report.stale_tasks_advanced, 1);
        assert_eq!(next_run_of(&db, "boundary").await, Some(now - 7 * 86_400));
    }

    /// Tasks under 7 days overdue must NOT be advanced — that's the
    /// dispatcher's normal due window, not the wake-from-sleep recovery
    /// case. Guards against a regression that would burst-replay every
    /// missed window after a short pause.
    #[tokio::test]
    async fn drift_recovery_leaves_recent_overdue_alone() {
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        let recent = now - 6 * 86_400;
        seed_interval_task(&db, "recent", Some(recent)).await;

        let report = drift_recovery(&db).await;
        assert_eq!(report.stale_tasks_advanced, 0);
        assert_eq!(next_run_of(&db, "recent").await, Some(recent));
    }

    /// Mixed batch: stale advances, fresh stays. Guards against an
    /// implementation that mutates every row in the due-task list.
    #[tokio::test]
    async fn drift_recovery_mixed_batch_only_advances_stale() {
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        let stale_ts = now - 10 * 86_400;
        let fresh_ts = now - 60;

        seed_interval_task(&db, "stale", Some(stale_ts)).await;
        seed_interval_task(&db, "fresh", Some(fresh_ts)).await;

        let report = drift_recovery(&db).await;
        assert_eq!(report.stale_tasks_advanced, 1);
        assert_ne!(
            next_run_of(&db, "stale").await,
            Some(stale_ts),
            "stale task should have been advanced"
        );
        assert_eq!(
            next_run_of(&db, "fresh").await,
            Some(fresh_ts),
            "fresh task must not be touched"
        );
    }

    async fn seed_once_task(db: &Database, id: &str, next_run: Option<i64>) {
        db.create_task(&NewTask {
            id,
            name: id,
            prompt: "noop",
            schedule_type: "once",
            schedule_expr: "2026-01-01T00:00:00Z",
            timezone: "UTC",
            next_run,
            max_retries: Some(0),
            timeout_ms: Some(60_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("create_task");
    }

    #[tokio::test]
    async fn a_long_overdue_one_shot_is_left_to_run_late() {
        // The bug: `advance_next_run` on a one-shot sets status=completed and
        // next_run=NULL. Feeding overdue one-shots through the stale-task
        // suppression therefore closed a task the user asked for as "done"
        // having never run it once — with only a log line to show for it.
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        seed_once_task(&db, "reminder", Some(now - 30 * 86_400)).await;

        let advanced = advance_stale_tasks(&db).await;

        assert_eq!(advanced, 0, "a one-shot must not be advanced");
        let task = db
            .get_task_by_id("reminder")
            .await
            .expect("read task")
            .expect("task exists");
        assert_ne!(
            task.status, "completed",
            "a task that never ran must not be marked completed"
        );
        assert_eq!(
            task.next_run,
            Some(now - 30 * 86_400),
            "it must stay due so the next tick runs it late"
        );
    }

    #[tokio::test]
    async fn a_long_overdue_recurring_task_is_still_advanced() {
        // The suppression this preserves: a recurring task must not replay
        // every occurrence missed while the daemon was down.
        let db = test_db().await;
        let now = chrono::Utc::now().timestamp();
        seed_interval_task(&db, "nightly", Some(now - 30 * 86_400)).await;

        let advanced = advance_stale_tasks(&db).await;

        assert_eq!(advanced, 1);
        let next = next_run_of(&db, "nightly").await.expect("still armed");
        assert!(
            next > now - 86_400,
            "a recurring task must be re-armed near now, got {next}"
        );
    }
}
