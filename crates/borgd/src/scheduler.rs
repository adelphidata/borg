//! Scheduled-task dispatcher loop.
//!
//! Watches `task_runs` / `scheduled_tasks` tables and fires due tasks. This
//! mirrors the dispatcher embedded in `crates/cli/src/service/mod.rs::run_daemon`
//! — pulled out so `borgd` can run it without going through the legacy cli
//! path.
//!
//! Task execution itself (LLM call, command spawn, maintenance sweep) is
//! delegated back to the cli's `service::*` helpers via `borgd::run_legacy`.
//! This module is the loop body only.

use anyhow::Result;
use cortex::config::Config;
use cortex::constants::{
    DAEMON_LOCK_MAX_REFRESH_FAILURES, DAEMON_LOOP_INTERVAL, SLEEP_DRIFT_THRESHOLD,
    STALLED_TASK_GRACE_SECS, STALLED_TASK_SCAN_INTERVAL, WATCHDOG_STALL_THRESHOLD,
    WATCHDOG_TICK_INTERVAL,
};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Callback the dispatcher invokes for each claimed task. The cli's
/// `service::spawn_task_execution` gets passed in here.
pub type TaskSpawn = Arc<
    dyn Fn(
            cortex::db::ScheduledTaskRow,
            i64,
            bool,
            Arc<tokio::sync::Semaphore>,
            Config,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Workflow step spawn callback. Same shape — provided by the cli shim.
pub type WorkflowStepSpawn = Arc<
    dyn Fn(
            cortex::db::WorkflowRow,
            cortex::db::WorkflowStepRow,
            Vec<cortex::db::WorkflowStepRow>,
            Vec<cortex::db::WorkflowStepRow>,
            Arc<tokio::sync::Semaphore>,
            Config,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Celebration deliver callback.
pub type CelebrationDeliver = Arc<
    dyn Fn(Config, cortex::db::PendingCelebration) -> futures::future::BoxFuture<'static, bool>
        + Send
        + Sync,
>;

/// Hooks the dispatcher needs to run task bodies. The cli wires these to the
/// existing `service::*` async helpers; production callers from inside borgd
/// can supply their own implementation later.
#[derive(Clone)]
pub struct DispatcherHooks {
    /// Spawn a single scheduled-task execution. The dispatcher only knows
    /// about claiming + retry scheduling; the body is the cli's territory.
    pub spawn_task: TaskSpawn,
    /// Spawn a single workflow step.
    pub spawn_workflow_step: WorkflowStepSpawn,
    /// Deliver an evolution/milestone celebration.
    pub deliver_celebration: CelebrationDeliver,
}

/// Run the dispatcher loop until `shutdown` fires. Returns when shutdown is
/// observed — caller should drain any remaining permits.
pub async fn run(
    config: Config,
    hooks: DispatcherHooks,
    semaphore: Arc<tokio::sync::Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    let db = cortex::db::Database::open()?;

    let daemon_pid = std::process::id();
    let now = chrono::Utc::now().timestamp();
    if !db.acquire_daemon_lock(daemon_pid, now)? {
        anyhow::bail!(
            "Another daemon instance is already running. Only one daemon can run at a time."
        );
    }

    // Recover stale runs from a previous crash.
    if let Ok(count) = db.recover_stale_runs("Daemon crashed during execution") {
        if count > 0 {
            tracing::warn!("Recovered {count} stale task run(s) from previous daemon crash");
        }
    }
    if let Ok(count) = db.recover_stale_workflow_steps() {
        if count > 0 {
            tracing::warn!("Recovered {count} stale workflow step(s) from previous daemon crash");
        }
    }

    skip_stale_tasks(&db);

    let mut interval = tokio::time::interval(DAEMON_LOOP_INTERVAL);

    // Watchdog
    let watchdog_ts = Arc::new(AtomicI64::new(chrono::Utc::now().timestamp()));
    {
        let ts = watchdog_ts.clone();
        let wd_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut wd_interval = tokio::time::interval(WATCHDOG_TICK_INTERVAL);
            loop {
                tokio::select! {
                    biased;
                    _ = wd_shutdown.cancelled() => return,
                    _ = wd_interval.tick() => {}
                }
                let last = ts.load(Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp();
                if now - last > WATCHDOG_STALL_THRESHOLD.as_secs() as i64 {
                    tracing::error!(
                        "Daemon watchdog: dispatcher loop stale for {}s, exiting",
                        now - last
                    );
                    std::process::exit(1);
                }
            }
        });
    }

    let mut last_tick_wall = std::time::Instant::now();
    let mut last_stalled_scan = std::time::Instant::now();
    let mut lock_refresh_failures: u32 = 0;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                if let Err(e) = db.release_daemon_lock(daemon_pid) {
                    tracing::warn!(error = %e, "release daemon lock on shutdown failed");
                }
                return Ok(());
            }
            _ = interval.tick() => {}
        }

        watchdog_ts.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);

        let elapsed = last_tick_wall.elapsed();
        last_tick_wall = std::time::Instant::now();
        if elapsed > SLEEP_DRIFT_THRESHOLD {
            tracing::info!(
                "Daemon resumed after {}s pause, re-checking stale tasks",
                elapsed.as_secs()
            );
            skip_stale_tasks(&db);
        }

        let now = chrono::Utc::now().timestamp();

        match db.refresh_daemon_lock(daemon_pid, now) {
            Ok(()) => lock_refresh_failures = 0,
            Err(e) => {
                lock_refresh_failures += 1;
                tracing::warn!(
                    "Failed to refresh daemon lock ({lock_refresh_failures}/{DAEMON_LOCK_MAX_REFRESH_FAILURES}): {e}"
                );
                if lock_refresh_failures >= DAEMON_LOCK_MAX_REFRESH_FAILURES {
                    tracing::error!("Daemon lock lost, exiting");
                    shutdown.cancel();
                    continue;
                }
            }
        }

        if last_stalled_scan.elapsed() >= STALLED_TASK_SCAN_INTERVAL {
            last_stalled_scan = std::time::Instant::now();
            if let Err(e) = cortex::tasks::heal_stalled_tasks(&db, now, STALLED_TASK_GRACE_SECS) {
                tracing::warn!("Stalled-task scan failed: {e}");
            }
            if let Err(e) = cortex::tasks::recover_wedged_runs(&db, now, STALLED_TASK_GRACE_SECS) {
                tracing::warn!("Wedged-run sweep failed: {e}");
            }
        }

        // Claim due tasks.
        match db.claim_due_tasks(now) {
            Ok(claimed) => {
                for ct in claimed {
                    let fut = (hooks.spawn_task)(
                        ct.task,
                        ct.run_id,
                        false,
                        semaphore.clone(),
                        config.clone(),
                    );
                    fut.await;
                }
            }
            Err(e) => tracing::warn!("Failed to claim due tasks: {e}"),
        }

        // Retries.
        match db.get_tasks_pending_retry(now) {
            Ok(retries) => {
                for task in retries {
                    let run_id = match db.start_task_run(&task.id, now) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!("Failed to start task run for '{}': {e}", task.name);
                            continue;
                        }
                    };
                    let fut =
                        (hooks.spawn_task)(task, run_id, true, semaphore.clone(), config.clone());
                    fut.await;
                }
            }
            Err(e) => tracing::warn!("Failed to check retry tasks: {e}"),
        }

        // Workflow steps (one step per workflow per tick).
        match db.get_runnable_workflows() {
            Ok(workflows) => {
                for wf in workflows {
                    if let Ok(Some(step)) = db.claim_next_workflow_step(&wf.id) {
                        let prior = db.get_completed_workflow_steps(&wf.id).unwrap_or_default();
                        let all_steps = db.get_workflow_steps(&wf.id).unwrap_or_default();
                        let fut = (hooks.spawn_workflow_step)(
                            wf,
                            step,
                            prior,
                            all_steps,
                            semaphore.clone(),
                            config.clone(),
                        );
                        fut.await;
                    }
                }
            }
            Err(e) => tracing::warn!("Failed to check runnable workflows: {e}"),
        }

        // Celebrations.
        match db.claim_pending_celebrations() {
            Ok(celebrations) => {
                for c in celebrations {
                    let celebration_id = c.id;
                    let cfg = config.clone();
                    let deliver = hooks.deliver_celebration.clone();
                    tokio::spawn(async move {
                        let delivered = (deliver)(cfg, c).await;
                        match cortex::db::Database::open() {
                            Ok(cdb) => {
                                if delivered {
                                    if let Err(e) = cdb.mark_celebration_delivered(celebration_id) {
                                        tracing::warn!(
                                            "Failed to mark celebration {celebration_id} delivered: {e}"
                                        );
                                    }
                                } else if let Err(e) = cdb.unclaim_celebration(celebration_id) {
                                    tracing::warn!(
                                        "Failed to unclaim celebration {celebration_id}: {e}"
                                    );
                                }
                            }
                            Err(e) => tracing::warn!("Celebration: db reopen failed: {e}"),
                        }
                    });
                }
            }
            Err(e) => tracing::warn!("Failed to check pending celebrations: {e}"),
        }
    }
}

fn skip_stale_tasks(db: &cortex::db::Database) {
    let now = chrono::Utc::now().timestamp();
    if let Ok(stale_tasks) = db.get_due_tasks(now) {
        for task in &stale_tasks {
            let age = now - task.next_run.unwrap_or(0);
            if age > 7 * 86400 {
                tracing::warn!(
                    "Skipping stale task '{}' ({}d overdue)",
                    task.name,
                    age / 86400
                );
                if let Err(e) = cortex::tasks::advance_next_run(task, db) {
                    tracing::warn!(
                        error = %e,
                        task = %task.name,
                        "advance_next_run on stale task failed",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::skip_stale_tasks;
    use cortex::db::{Database, NewTask};

    fn test_db() -> Database {
        Database::from_connection(rusqlite::Connection::open_in_memory().expect("in-memory db"))
            .expect("init test db")
    }

    fn seed_interval_task(db: &Database, id: &str, next_run: Option<i64>) {
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
        })
        .expect("create_task");
    }

    fn next_run_of(db: &Database, id: &str) -> Option<i64> {
        db.get_task_by_id(id)
            .expect("read task")
            .expect("task exists")
            .next_run
    }

    #[test]
    fn task_overdue_more_than_seven_days_gets_advanced() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        let stale = now - 8 * 86_400;
        seed_interval_task(&db, "stale", Some(stale));

        skip_stale_tasks(&db);

        // Interval schedule recomputes to roughly now + 60s.
        let advanced = next_run_of(&db, "stale").expect("interval task always has next_run");
        assert!(
            advanced > now,
            "expected next_run > now after advance, got {advanced} (now={now})"
        );
    }

    /// Boundary: the predicate is `age > 7 * 86_400`, so exactly 7 days old
    /// must NOT advance. Catches a future `>=` regression.
    #[test]
    fn task_exactly_seven_days_overdue_is_unchanged() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        let boundary = now - 7 * 86_400;
        seed_interval_task(&db, "boundary", Some(boundary));

        skip_stale_tasks(&db);

        assert_eq!(
            next_run_of(&db, "boundary"),
            Some(boundary),
            "task at the 7-day boundary must not be advanced"
        );
    }

    #[test]
    fn task_under_seven_days_overdue_is_unchanged() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        let recent = now - 6 * 86_400;
        seed_interval_task(&db, "recent", Some(recent));

        skip_stale_tasks(&db);

        assert_eq!(next_run_of(&db, "recent"), Some(recent));
    }

    /// Mixed batch: stale tasks advance, fresh tasks don't. Guards against an
    /// implementation that mutates every row in the due-task list.
    #[test]
    fn only_stale_tasks_in_a_mixed_batch_are_advanced() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp();
        let stale_ts = now - 10 * 86_400;
        let fresh_ts = now - 60; // due, but not stale

        seed_interval_task(&db, "stale", Some(stale_ts));
        seed_interval_task(&db, "fresh", Some(fresh_ts));

        skip_stale_tasks(&db);

        assert_ne!(
            next_run_of(&db, "stale"),
            Some(stale_ts),
            "stale task should have been advanced"
        );
        assert_eq!(
            next_run_of(&db, "fresh"),
            Some(fresh_ts),
            "fresh task must not be touched"
        );
    }
}
