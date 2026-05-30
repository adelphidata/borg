//! Scheduled-task dispatcher loop.
//!
//! Watches `task_runs` / `scheduled_tasks` and fires due tasks. Loop body only —
//! task execution (LLM call, command spawn, maintenance sweep) is delegated to
//! the `service::*` helpers via `borgd::run_legacy`.

use crate::maintenance::sweep;
use crate::paths::tick_lock_path;
use crate::tick_lock::TickLock;
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
    let db = cortex::db::Database::open().await?;

    let daemon_pid = std::process::id();
    let now = chrono::Utc::now().timestamp();
    if !db.acquire_daemon_lock(daemon_pid, now).await? {
        anyhow::bail!(
            "Another daemon instance is already running. Only one daemon can run at a time."
        );
    }

    // Resolve the per-tick advisory lock path once. The lock itself is
    // acquired and released *inside* each iteration of the dispatch loop —
    // see the claim+dispatch arm below for rationale.
    let tick_lock_file = tick_lock_path(&Config::data_dir()?);

    // Boot-time recovery: in-flight task runs from a crashed previous
    // daemon are marked failed, and any task more than seven days
    // overdue gets its `next_run` advanced so we don't replay every
    // missed window in a burst. Concentrated into the maintenance
    // module — see `crates/borgd/src/maintenance/sweep.rs`.
    let boot_report = sweep::boot_recovery(&db).await;
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
                        elapsed_secs = now - last,
                        threshold_secs = WATCHDOG_STALL_THRESHOLD.as_secs(),
                        "Daemon watchdog: dispatcher loop stale, exiting"
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
        // `biased` ensures shutdown wins over a coincident tick, so SIGTERM
        // drains within one interval rather than running one final dispatch.
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                if let Err(e) = db.release_daemon_lock(daemon_pid).await {
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
            let drift_report = sweep::drift_recovery(&db).await;
            if drift_report.stale_tasks_advanced > 0 {
                tracing::info!(
                    advanced = drift_report.stale_tasks_advanced,
                    "drift recovery: advanced stale tasks after wake-from-sleep",
                );
            }
        }

        let now = chrono::Utc::now().timestamp();

        match db.refresh_daemon_lock(daemon_pid, now).await {
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
            let scan = sweep::stalled_scan(&db, now, STALLED_TASK_GRACE_SECS).await;
            if scan.tasks_healed > 0 || scan.wedged_runs_recovered > 0 {
                tracing::info!(
                    healed = scan.tasks_healed,
                    wedged = scan.wedged_runs_recovered,
                    "stalled-task scan made repairs",
                );
            }
        }

        // Claim+dispatch critical section. The non-blocking tick lock makes
        // "no two ticks run concurrently" an explicit, observable property:
        //
        //  - SQLite's `BEGIN IMMEDIATE` already serializes concurrent
        //    `claim_due_tasks` callers, but only at the row-write boundary —
        //    the surrounding hook spawns are unprotected.
        //  - If a tick stalls past `DAEMON_LOOP_INTERVAL`, the next
        //    `interval.tick()` would otherwise re-enter this arm immediately.
        //  - Out-of-process tick callers (manual CLI, future ACP triggers)
        //    bypass the in-process loop sequencing entirely.
        //
        // Collision → debug log + skip; the next interval (60s) retries
        // naturally because `next_run` was never advanced. The lock is
        // dropped before re-entering `interval.tick().await`, so it does
        // not extend across the inter-tick sleep.
        let _tick_lock = match TickLock::try_acquire(&tick_lock_file) {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::debug!(
                    path = %tick_lock_file.display(),
                    "scheduler tick skipped — another tick holds the lock"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %tick_lock_file.display(),
                    "scheduler tick lock open failed; skipping iteration"
                );
                continue;
            }
        };

        // Claim due tasks.
        match db.claim_due_tasks(now).await {
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
        match db.get_tasks_pending_retry(now).await {
            Ok(retries) => {
                for task in retries {
                    let run_id = match db.start_task_run(&task.id, now).await {
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
        match db.claim_pending_celebrations().await {
            Ok(celebrations) => {
                for c in celebrations {
                    let celebration_id = c.id;
                    let cfg = config.clone();
                    let deliver = hooks.deliver_celebration.clone();
                    tokio::spawn(async move {
                        let delivered = (deliver)(cfg, c).await;
                        match cortex::db::Database::open().await {
                            Ok(cdb) => {
                                if delivered {
                                    if let Err(e) =
                                        cdb.mark_celebration_delivered(celebration_id).await
                                    {
                                        tracing::warn!(
                                            "Failed to mark celebration {celebration_id} delivered: {e}"
                                        );
                                    }
                                } else if let Err(e) = cdb.unclaim_celebration(celebration_id).await
                                {
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

        // `_tick_lock` drops here at end of iteration, before the next
        // `interval.tick().await`.
    }
}

// `skip_stale_tasks` and its boundary tests now live in
// `crate::maintenance::sweep` — the dispatcher loop calls into that
// module instead of inlining the recovery logic.
