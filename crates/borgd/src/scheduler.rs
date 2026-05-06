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

use crate::maintenance::sweep;
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

    // Boot-time recovery: in-flight task runs from a crashed previous
    // daemon are marked failed, and any task more than seven days
    // overdue gets its `next_run` advanced so we don't replay every
    // missed window in a burst. Concentrated into the maintenance
    // module — see `crates/borgd/src/maintenance/sweep.rs`.
    let boot_report = sweep::boot_recovery(&db);
    if boot_report.stale_tasks_advanced > 0 {
        tracing::info!(
            advanced = boot_report.stale_tasks_advanced,
            "boot recovery: advanced stale tasks past 7-day overdue threshold",
        );
    }

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
            let drift_report = sweep::drift_recovery(&db);
            if drift_report.stale_tasks_advanced > 0 {
                tracing::info!(
                    advanced = drift_report.stale_tasks_advanced,
                    "drift recovery: advanced stale tasks after wake-from-sleep",
                );
            }
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
            let scan = sweep::stalled_scan(&db, now, STALLED_TASK_GRACE_SECS);
            if scan.tasks_healed > 0 || scan.wedged_runs_recovered > 0 {
                tracing::info!(
                    healed = scan.tasks_healed,
                    wedged = scan.wedged_runs_recovered,
                    "stalled-task scan made repairs",
                );
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

// `skip_stale_tasks` and its boundary tests now live in
// `crate::maintenance::sweep` — the dispatcher loop calls into that
// module instead of inlining the recovery logic.
