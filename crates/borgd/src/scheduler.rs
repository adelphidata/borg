//! Scheduled-task dispatcher loop.
//!
//! Watches `task_runs` / `scheduled_tasks` and fires due tasks. Loop body only —
//! task execution (LLM call, command spawn, maintenance sweep) is delegated to
//! the `service::*` helpers via `borgd::run_legacy`.

use crate::maintenance::sweep;
use crate::tick_lock::TickLock;
use anyhow::Result;
use cortex::config::Config;
use cortex::constants::{
    DAEMON_LOCK_STALE_SECS, DAEMON_LOOP_INTERVAL, SLEEP_DRIFT_THRESHOLD, STALLED_TASK_GRACE_SECS,
    STALLED_TASK_SCAN_INTERVAL,
};
use cortex::paths::tick_lock_path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// OS error codes that mean the disk, not the tick, is the problem.
#[cfg(unix)]
const STORAGE_ERRNOS: &[i32] = &[libc::ENOSPC, libc::EDQUOT, libc::EROFS];
#[cfg(not(unix))]
const STORAGE_ERRNOS: &[i32] = &[libc::ENOSPC, libc::EROFS];

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

/// Asks the whole process to stop — not just the orchestrator subsystems.
///
/// The dispatcher calls this when it can no longer prove it holds the daemon
/// lock. Cancelling only the orchestrator token would stop the gateway,
/// heartbeat and dispatcher while the gRPC servers keep the process alive
/// half-dead; a full stop lets launchd/systemd restart a whole daemon.
pub type ProcessShutdown = Arc<dyn Fn() + Send + Sync>;

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
    /// Stop the entire process (see [`ProcessShutdown`]).
    pub shutdown_process: ProcessShutdown,
}

/// Boot recovery runs once per process, not once per dispatcher incarnation.
///
/// The dispatcher is supervised: a wedge aborts and respawns `run`, but the
/// task futures the previous incarnation spawned keep executing in this
/// process. Re-running recovery would fail those live runs and reclaim the
/// session leases this very process still holds.
static BOOT_RECOVERY_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim the one boot-recovery pass this process gets. `true` for the first
/// caller only.
fn claim_boot_recovery() -> bool {
    !BOOT_RECOVERY_CLAIMED.swap(true, Ordering::SeqCst)
}

// ── Daemon-lock refresh policy ──

/// What one `refresh_daemon_lock` call told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockRefresh {
    /// Our row was stamped; we provably still hold the lock.
    Refreshed,
    /// No row matched our pid: stolen or released. Another daemon may be
    /// dispatching right now.
    Lost,
    /// The store refused the write (busy timeout, I/O). Says nothing about
    /// who holds the lock.
    Transient,
}

impl LockRefresh {
    pub(crate) fn from_result(result: &anyhow::Result<bool>) -> Self {
        match result {
            Ok(true) => Self::Refreshed,
            Ok(false) => Self::Lost,
            Err(_) => Self::Transient,
        }
    }
}

/// The dispatcher's decision after a refresh attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockVerdict {
    Continue,
    /// Stop dispatching and ask the process to exit.
    Cancel,
}

/// Safety margin, in seconds, kept between the last successful refresh and the
/// moment another daemon could legally take the lock
/// (`DAEMON_LOCK_STALE_SECS`). The dispatcher gives up on transient store
/// errors *before* that moment, so two daemons never dispatch at once.
///
/// Two loop intervals: one for the tick that discovers the budget is gone,
/// one for the shutdown to drain.
pub(crate) const LOCK_GIVE_UP_MARGIN_SECS: i64 = 2 * DAEMON_LOOP_INTERVAL.as_secs() as i64;

const _: () = assert!(
    LOCK_GIVE_UP_MARGIN_SECS < DAEMON_LOCK_STALE_SECS,
    "the give-up margin must leave a positive budget for transient refresh errors"
);

/// Pure policy for lock-refresh outcomes. Tolerates transient store errors
/// for as long as the row cannot yet have gone stale; a lost lock is fatal at
/// once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LockRefreshPolicy {
    /// Unix seconds of the last refresh that provably stamped our row (or the
    /// acquire, before any refresh has run).
    last_ok_at: i64,
}

impl LockRefreshPolicy {
    pub(crate) fn new(acquired_at: i64) -> Self {
        Self {
            last_ok_at: acquired_at,
        }
    }

    /// Seconds of transient errors tolerated after the last good refresh.
    pub(crate) const fn transient_budget_secs() -> i64 {
        DAEMON_LOCK_STALE_SECS - LOCK_GIVE_UP_MARGIN_SECS
    }

    pub(crate) fn observe(&mut self, outcome: LockRefresh, now: i64) -> LockVerdict {
        match outcome {
            LockRefresh::Refreshed => {
                self.last_ok_at = now;
                LockVerdict::Continue
            }
            LockRefresh::Lost => LockVerdict::Cancel,
            LockRefresh::Transient => {
                if now.saturating_sub(self.last_ok_at) < Self::transient_budget_secs() {
                    LockVerdict::Continue
                } else {
                    LockVerdict::Cancel
                }
            }
        }
    }
}

// ── Tick failure classification ──

/// Why a tick could not reach a dispatch decision, grouped by what a restart
/// would do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickFailure {
    /// The store is full or another writer holds it. The loop itself is fine
    /// and a restart changes nothing — it would re-run `Database::open` and
    /// `acquire_daemon_lock` against the same store, trip the supervisor's
    /// restart cap, and leave the daemon without a dispatcher for good. The
    /// tick counts as alive; the warning is throttled per class.
    HealthySkip(TickFailureClass),
    /// Anything else — descriptor exhaustion above all — where the restart
    /// *is* the fix (it frees what the loop leaked). Liveness is withheld so
    /// the watchdog restarts the subsystem.
    Unhealthy,
}

/// The healthy-skip classes, each with its own warning throttle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickFailureClass {
    StorageExhausted,
    LockContention,
}

impl TickFailureClass {
    const COUNT: usize = 2;

    fn index(self) -> usize {
        match self {
            Self::StorageExhausted => 0,
            Self::LockContention => 1,
        }
    }

    fn operator_hint(self) -> &'static str {
        match self {
            Self::StorageExhausted => {
                "the data store is full or read-only; no scheduled task can run until space is freed"
            }
            Self::LockContention => {
                "another process holds the database write lock; scheduled tasks are deferred until it releases"
            }
        }
    }
}

/// Classify a failed tick. Walks the error chain for the raw storage errnos:
/// the tick-lock path wraps its I/O error in context, and `cortex::db`'s
/// predicates look only at the top-level error.
pub(crate) fn classify_tick_failure(e: &anyhow::Error) -> TickFailure {
    if cortex::db::is_storage_exhausted(e) {
        return TickFailure::HealthySkip(TickFailureClass::StorageExhausted);
    }
    if cortex::db::is_lock_contention(e) {
        return TickFailure::HealthySkip(TickFailureClass::LockContention);
    }
    let storage_errno = e
        .chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .filter_map(std::io::Error::raw_os_error)
        .any(|errno| STORAGE_ERRNOS.contains(&errno));
    if storage_errno {
        return TickFailure::HealthySkip(TickFailureClass::StorageExhausted);
    }
    TickFailure::Unhealthy
}

/// Minimum spacing between two warnings of the same healthy-skip class.
pub(crate) const HEALTHY_SKIP_WARN_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Per-class warning throttle for healthy skips. A full disk fails every tick
/// identically; one warning per class per quarter hour says the same thing
/// without burying the log.
#[derive(Debug, Default)]
pub(crate) struct HealthySkipWarnThrottle {
    last_warned: [Option<Instant>; TickFailureClass::COUNT],
}

impl HealthySkipWarnThrottle {
    /// Whether to emit a warning for `class` at `now`; records it if so.
    pub(crate) fn should_warn(&mut self, class: TickFailureClass, now: Instant) -> bool {
        let slot = &mut self.last_warned[class.index()];
        match slot {
            Some(last) if now.duration_since(*last) < HEALTHY_SKIP_WARN_INTERVAL => false,
            _ => {
                *slot = Some(now);
                true
            }
        }
    }
}

/// Apply a failed tick's classification: a healthy skip counts as alive and
/// warns at most once per class per [`HEALTHY_SKIP_WARN_INTERVAL`]; an
/// unhealthy failure leaves the liveness counter alone so the watchdog can
/// restart the loop.
fn note_tick_failure(
    failure: TickFailure,
    liveness: &AtomicU64,
    throttle: &mut HealthySkipWarnThrottle,
) {
    let TickFailure::HealthySkip(class) = failure else {
        return;
    };
    liveness.fetch_add(1, Ordering::Relaxed);
    if throttle.should_warn(class, Instant::now()) {
        tracing::warn!(
            class = ?class,
            "scheduler tick skipped: {}. A restart would not help, so the dispatcher stays up \
             and retries; this warning repeats at most every {} minutes",
            class.operator_hint(),
            HEALTHY_SKIP_WARN_INTERVAL.as_secs() / 60
        );
    }
}

/// Notice, and record, that the previous daemon never released its lock.
///
/// `daemon_lock` already encodes this for free: the graceful shutdown arm
/// DELETEs the row, so a surviving row at boot means the last process died
/// without reaching it — OOM kill, SIGKILL, or host reset. The pidlock file
/// cannot substitute: it is deliberately not unlinked on drop and is
/// unconditionally rewritten on acquire, so a clean exit and a SIGKILL leave
/// it byte-identical.
///
/// Returns the dead process's last heartbeat timestamp when an unclean exit
/// was detected. Best-effort: a DB hiccup here must not block boot.
pub(crate) async fn report_prior_unclean_exit(
    db: &cortex::db::Database,
    daemon_pid: u32,
    now: i64,
) -> Option<i64> {
    let prior = match db.daemon_lock_snapshot().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not read daemon lock for unclean-exit check");
            return None;
        }
    };
    // Same PID is a re-exec, which `acquire_daemon_lock_inner` already treats
    // as a restart; PID reuse makes it indistinguishable from a real crash.
    let (pid, heartbeat_at) = prior.filter(|(pid, _)| *pid != daemon_pid)?;

    tracing::warn!(
        prior_pid = pid,
        last_heartbeat_unix = heartbeat_at,
        downtime_secs = now.saturating_sub(heartbeat_at),
        "previous borgd exited uncleanly (daemon lock was never released) — \
         in-flight task runs and unflushed session state may have been lost"
    );

    // The lock row is overwritten microseconds from now by
    // `acquire_daemon_lock`, so the finding has to be durable somewhere a
    // later `borg doctor` process can read it.
    if let Err(e) = db
        .set_meta(
            cortex::constants::META_LAST_UNCLEAN_SHUTDOWN_AT,
            &heartbeat_at.to_string(),
        )
        .await
    {
        tracing::warn!(error = %e, "recording unclean shutdown timestamp failed");
    }

    cortex::db::try_emit_global(
        cortex::db::audit_events::DAEMON_UNCLEAN_EXIT,
        "system",
        &serde_json::json!({
            "prior_pid": pid,
            "last_heartbeat_unix": heartbeat_at,
            "downtime_secs": now.saturating_sub(heartbeat_at),
        }),
    )
    .await;

    Some(heartbeat_at)
}

/// Longest a failing tick will wait before trying again. Past this the
/// watchdog has had ample time to notice the missing liveness bumps.
///
/// MUST stay comfortably below `DAEMON_LOCK_STALE_SECS` (300s): the daemon
/// lock heartbeat is only refreshed at the top of the loop, so a backoff at
/// the cap is a window in which the lock goes unrefreshed. If the cap ever
/// reached the stale threshold, a concurrently started `borgd` could steal
/// the lock mid-backoff and both daemons would dispatch.
const MAX_TICK_BACKOFF: std::time::Duration = std::time::Duration::from_secs(120);

const _: () = assert!(
    MAX_TICK_BACKOFF.as_secs() * 2 <= cortex::constants::DAEMON_LOCK_STALE_SECS as u64,
    "tick backoff must leave the daemon-lock heartbeat well inside its stale threshold"
);

/// Whether an IO error is the process running out of file descriptors.
///
/// Worth naming specifically: it is self-inflicted, it makes every later tick
/// fail identically, and the fix is an operator action rather than a retry.
fn is_fd_exhaustion(e: &anyhow::Error) -> bool {
    // EMFILE (per-process) and ENFILE (system-wide). `ErrorKind` has no
    // stable variant for either, so match the raw codes off the underlying
    // IO error wherever it sits in the chain.
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| matches!(io.raw_os_error(), Some(24) | Some(23)))
}

/// How long to wait before retrying after a tick that could not dispatch.
///
/// Exponential from the loop interval, capped. A scheduler hammering a
/// resource it has already exhausted only makes the exhaustion worse.
fn tick_backoff_delay(consecutive_failures: u32) -> std::time::Duration {
    let shift = consecutive_failures.saturating_sub(1).min(8);
    DAEMON_LOOP_INTERVAL
        .saturating_mul(1u32 << shift)
        .min(MAX_TICK_BACKOFF)
}

/// Wait out the backoff, or return early if the daemon is shutting down.
///
/// Returns `true` when shutdown was requested. The wait has to be
/// cancellable: at the cap it is two minutes, and the loop only checks the
/// shutdown token at the top of its `select!`. An uncancellable sleep would
/// hold a SIGTERM for that long — and since the shutdown arm is what releases
/// the daemon lock and stamps the clean-exit timestamp, the process would
/// almost certainly be killed first and the next boot would report an unclean
/// exit that never happened.
async fn backoff_after_failed_tick(
    consecutive_failures: u32,
    shutdown: &CancellationToken,
) -> bool {
    let delay = tick_backoff_delay(consecutive_failures);
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// The dispatcher's clean-stop bookkeeping: release the daemon lock and
/// stamp the clean-shutdown time, so the doctor's unclean-exit warning
/// clears itself on a graceful stop rather than needing a squelch. Runs on
/// the shutdown arm and before a lock-loss process stop.
async fn record_clean_shutdown(db: &cortex::db::Database, daemon_pid: u32) {
    if let Err(e) = db.release_daemon_lock(daemon_pid).await {
        tracing::warn!(error = %e, "release daemon lock on shutdown failed");
    }
    if let Err(e) = db
        .set_meta(
            cortex::constants::META_LAST_CLEAN_SHUTDOWN_AT,
            &chrono::Utc::now().timestamp().to_string(),
        )
        .await
    {
        tracing::warn!(error = %e, "recording clean shutdown timestamp failed");
    }
}

/// Run the dispatcher loop until `shutdown` fires. Returns when shutdown is
/// observed — caller should drain any remaining permits.
pub async fn run(
    config: Config,
    hooks: DispatcherHooks,
    semaphore: Arc<tokio::sync::Semaphore>,
    shutdown: CancellationToken,
    liveness: Arc<AtomicU64>,
) -> Result<()> {
    let db = cortex::db::Database::open().await?;

    let daemon_pid = std::process::id();
    let now = chrono::Utc::now().timestamp();

    // MUST run before `acquire_daemon_lock` — acquiring overwrites the very
    // row this reads.
    report_prior_unclean_exit(&db, daemon_pid, now).await;

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
    //
    // Once per process: a supervisor respawn of this loop must not fail the
    // runs its previous incarnation spawned (see `BOOT_RECOVERY_CLAIMED`).
    // Deliberately after `acquire_daemon_lock`: another live daemon's
    // in-flight rows are its own.
    if claim_boot_recovery() {
        let boot_report = sweep::boot_recovery(&db).await;
        if boot_report.stale_tasks_advanced > 0 {
            tracing::info!(
                advanced = boot_report.stale_tasks_advanced,
                "boot recovery: advanced stale tasks past 7-day overdue threshold",
            );
        }
    } else {
        tracing::debug!("dispatcher respawned; boot recovery already ran in this process");
    }

    let mut interval = tokio::time::interval(DAEMON_LOOP_INTERVAL);
    // Skip missed ticks instead of bursting. The default (`Burst`) replays
    // every tick missed during a backoff sleep back to back — the exact
    // "hammer the resource that just failed" behavior the backoff exists to
    // prevent.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Stall detection lives in the supervisor now: `liveness` is bumped once
    // per loop iteration below, and `spawn_supervised(..., liveness)` arms a
    // watchdog against *this* incarnation that aborts it if the counter stops
    // advancing. The dispatcher used to run its own watchdog that called
    // `std::process::exit(1)` — that killed the whole daemon for a stall in
    // one subsystem, and, because it was keyed to the daemon-wide shutdown
    // token, a supervised respawn left the old watchdog alive holding a
    // frozen timestamp, so any transient error became a hard kill 180s later.
    // Aborting and restarting the one wedged subsystem is both safer and what
    // the supervisor already knows how to do.

    let mut last_tick_wall = std::time::Instant::now();
    let mut last_stalled_scan = std::time::Instant::now();
    let mut lock_policy = LockRefreshPolicy::new(now);
    let mut skip_warn_throttle = HealthySkipWarnThrottle::default();
    // Consecutive ticks that could not reach a dispatch decision. Drives the
    // backoff below and, by withholding the liveness bump, lets the
    // supervisor's watchdog restart a scheduler that is stuck.
    let mut tick_failures: u32 = 0;
    // Emergency-stop edge tracker: log once per engagement, not per tick.
    let mut pause_gate = cortex::emergency_pause::PauseGate::new();

    loop {
        // `biased` ensures shutdown wins over a coincident tick, so SIGTERM
        // drains within one interval rather than running one final dispatch.
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                record_clean_shutdown(&db, daemon_pid).await;
                return Ok(());
            }
            _ = interval.tick() => {}
        }

        // NOTE: the liveness heartbeat is deliberately NOT bumped here.
        // Bumping at the top of the loop reports "alive" before anything has
        // been attempted, so a scheduler that ticks forever without ever
        // dispatching — fd exhaustion at the tick lock, a read-only data dir —
        // looks perfectly healthy to the watchdog and silently runs nothing.
        // Every path that actually reached a dispatch decision bumps it
        // instead; the failure paths do not, so the watchdog restarts the
        // subsystem, which is also what frees leaked descriptors.

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

        let refresh = db.refresh_daemon_lock(daemon_pid, now).await;
        let outcome = LockRefresh::from_result(&refresh);
        if let Err(e) = &refresh {
            tracing::warn!(
                error = %e,
                budget_secs = LockRefreshPolicy::transient_budget_secs(),
                "daemon lock refresh failed; tolerated until the lock could go stale"
            );
        }
        if lock_policy.observe(outcome, now) == LockVerdict::Cancel {
            match outcome {
                LockRefresh::Lost => tracing::error!(
                    "daemon lock lost to another process — stopping this daemon so only one dispatches"
                ),
                _ => tracing::error!(
                    "daemon lock could not be refreshed for {}s — stopping this daemon before \
                     another could take the lock; the service manager restarts it",
                    LockRefreshPolicy::transient_budget_secs()
                ),
            }
            // Release and stamp *before* asking the process to stop: a fast
            // exit must not leave the lock row behind as an unclean-shutdown
            // marker. Then a whole-process stop, so the service manager
            // restarts a complete daemon rather than leaving one with dead
            // subsystems behind live gRPC servers. The token cancel covers
            // callers whose process hook is a no-op; the shutdown work is
            // already done, so this loop simply ends.
            record_clean_shutdown(&db, daemon_pid).await;
            (hooks.shutdown_process)();
            shutdown.cancel();
            return Ok(());
        }

        // Emergency stop (`borg pause`): skip the entire claim+dispatch arm
        // while engaged. `claim_due_tasks` is never called, so `next_run`
        // stays due and every run is deferred — not lost — until resume.
        // The lock refresh and drift recovery deliberately keep running:
        // pause halts NEW agent work only, and maintenance is not agent work.
        // The stalled scan, however, runs *after* this gate: it re-arms
        // overdue recurring tasks to their next occurrence and records the
        // skipped fires as missed, which is exactly the "deferred, not lost"
        // promise above being broken by the pause outlasting the scan's
        // grace window. The read goes straight to the DB (fail-safe:
        // corrupt/unreadable flag ⇒ paused) so a CLI write from another
        // process gates the very next tick.
        if cortex::emergency_pause::is_engaged(&db).await {
            if let Some(cortex::emergency_pause::PauseTransition::Engaged) =
                pause_gate.observe(true)
            {
                tracing::warn!(
                    "emergency pause engaged — scheduled-task dispatch suspended; \
                     due runs are deferred until `borg resume`"
                );
            }
            // A deliberate no-dispatch is a healthy tick.
            liveness.fetch_add(1, Ordering::Relaxed);
            tick_failures = 0;
            continue;
        } else if let Some(cortex::emergency_pause::PauseTransition::Released) =
            pause_gate.observe(false)
        {
            tracing::info!("emergency pause released — scheduled-task dispatch resumed");
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
                // Contention means some other tick is doing the work; this
                // loop is healthy.
                liveness.fetch_add(1, Ordering::Relaxed);
                tick_failures = 0;
                continue;
            }
            Err(e) => {
                // NOT contention: the lock file could not be opened at all.
                // Descriptor exhaustion is the case that matters, because it
                // is self-inflicted, silent, and persistent — every later
                // tick fails the same way.
                tick_failures = tick_failures.saturating_add(1);
                let exhausted = is_fd_exhaustion(&e);
                tracing::warn!(
                    error = %e,
                    path = %tick_lock_file.display(),
                    consecutive_failures = tick_failures,
                    fd_exhaustion = exhausted,
                    "scheduler tick lock open failed; no task can be dispatched this tick"
                );
                if exhausted && tick_failures == 1 {
                    tracing::error!(
                        "scheduler cannot open its tick lock: the process is out of file \
                         descriptors. No scheduled task will run until this clears. Raise the \
                         open-file limit (`ulimit -n`) for the daemon."
                    );
                }
                note_tick_failure(
                    classify_tick_failure(&e),
                    &liveness,
                    &mut skip_warn_throttle,
                );
                // A shutdown during the wait falls through to the loop
                // top, where the `select!` runs the shutdown arm.
                backoff_after_failed_tick(tick_failures, &shutdown).await;
                continue;
            }
        };

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
            Err(e) => {
                // The database refused the claim: nothing was dispatched, so
                // this tick did not do its job. Counting it keeps a
                // persistently unusable DB from reading as a healthy loop —
                // unless the cause is one a restart cannot fix (full disk,
                // write-lock contention), where the loop is alive and only
                // the store is unwell.
                tick_failures = tick_failures.saturating_add(1);
                let failure = classify_tick_failure(&e);
                if failure == TickFailure::Unhealthy {
                    tracing::warn!(
                        consecutive_failures = tick_failures,
                        "Failed to claim due tasks: {e}"
                    );
                }
                note_tick_failure(failure, &liveness, &mut skip_warn_throttle);
                // This also skips the retry and celebration passes below for
                // this tick. Deliberate: they use the same database that just
                // refused the claim, so they would fail too — and each failure
                // would be logged as if it were an independent problem.
                // Release the tick lock before sleeping: holding it across a
                // multi-minute backoff would lock out out-of-process tick
                // callers (manual CLI, ACP triggers) even after the DB
                // recovers, breaking the "dropped before the inter-tick
                // sleep" contract documented above.
                drop(_tick_lock);
                // A shutdown during the wait falls through to the loop
                // top, where the `select!` runs the shutdown arm.
                backoff_after_failed_tick(tick_failures, &shutdown).await;
                continue;
            }
        }

        // A tick that reached and completed the claim did its job.
        liveness.fetch_add(1, Ordering::Relaxed);
        tick_failures = 0;

        // Stalled scan — after the claim, never before it. The scan re-arms
        // overdue recurring tasks to their next occurrence and records the
        // skipped fires as missed. Run ahead of the claim, a store that
        // refused claims for longer than the grace window would have every
        // due task re-armed the moment it recovered, instead of fired once.
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

#[cfg(test)]
mod unclean_exit_tests {
    use super::*;

    /// Encodes the contract the boot-time check depends on: the lock row is
    /// the crash signal. Fails if `acquire_daemon_lock` ever starts clearing
    /// on conflict, or if shutdown stops releasing.
    #[tokio::test]
    async fn daemon_lock_row_survives_unclean_exit_but_not_a_clean_one() {
        let tmp = tempfile::tempdir().unwrap();
        let db = cortex::db::Database::open_at(&tmp.path().join("borg.db"))
            .await
            .expect("open db");

        assert!(db.acquire_daemon_lock(100, 1_000).await.expect("acquire"));
        assert!(
            db.daemon_lock_snapshot().await.expect("snapshot").is_some(),
            "a held lock must be visible"
        );

        db.release_daemon_lock(100).await.expect("release");
        assert!(
            db.daemon_lock_snapshot().await.expect("snapshot").is_none(),
            "a clean shutdown must leave no lock row"
        );

        // Acquire again and "die" without releasing.
        assert!(db.acquire_daemon_lock(100, 2_000).await.expect("acquire"));
        assert_eq!(
            db.daemon_lock_snapshot().await.expect("snapshot"),
            Some((100, 2_000)),
            "an unclean exit must leave the row behind for the next boot"
        );
    }

    #[tokio::test]
    async fn report_prior_unclean_exit_records_the_finding() {
        let tmp = tempfile::tempdir().unwrap();
        let db = cortex::db::Database::open_at(&tmp.path().join("borg.db"))
            .await
            .expect("open db");

        // Nothing to report on a first-ever boot.
        assert_eq!(report_prior_unclean_exit(&db, 200, 5_000).await, None);
        assert_eq!(
            db.get_meta(cortex::constants::META_LAST_UNCLEAN_SHUTDOWN_AT)
                .await
                .expect("get_meta"),
            None,
            "a clean first boot must not write the meta key"
        );

        // A dead predecessor's row is still there.
        db.acquire_daemon_lock(100, 2_000).await.expect("acquire");
        assert_eq!(
            report_prior_unclean_exit(&db, 200, 5_000).await,
            Some(2_000)
        );
        assert_eq!(
            db.get_meta(cortex::constants::META_LAST_UNCLEAN_SHUTDOWN_AT)
                .await
                .expect("get_meta")
                .as_deref(),
            Some("2000"),
            "the finding must outlive the lock row that acquire is about to overwrite"
        );
    }

    #[tokio::test]
    async fn same_pid_restart_is_not_reported_as_unclean() {
        // `acquire_daemon_lock_inner` already treats a same-PID row as a
        // re-exec, and PID reuse makes it indistinguishable from a crash.
        let tmp = tempfile::tempdir().unwrap();
        let db = cortex::db::Database::open_at(&tmp.path().join("borg.db"))
            .await
            .expect("open db");
        db.acquire_daemon_lock(100, 2_000).await.expect("acquire");
        assert_eq!(report_prior_unclean_exit(&db, 100, 5_000).await, None);
    }
}

#[cfg(test)]
mod tick_health_tests {
    use super::*;

    // ── failed-tick classification ──

    #[test]
    fn descriptor_exhaustion_is_recognized_through_the_error_chain() {
        // Why it is worth naming: it is self-inflicted, every later tick
        // fails identically, and the fix is an operator action rather than a
        // retry. The lock error arrives wrapped, so matching only the
        // top-level error would miss it.
        for (code, expected) in [(24, true), (23, true), (2, false), (13, false)] {
            let io = std::io::Error::from_raw_os_error(code);
            let wrapped = anyhow::Error::new(io).context("open scheduler tick lock");
            assert_eq!(
                is_fd_exhaustion(&wrapped),
                expected,
                "raw os error {code} classified wrongly"
            );
        }
    }

    #[test]
    fn a_non_io_failure_is_not_mistaken_for_descriptor_exhaustion() {
        let err = anyhow::anyhow!("database is locked");
        assert!(!is_fd_exhaustion(&err));
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_failures_back_off_and_stay_bounded() {
        // A scheduler hammering a resource it has already exhausted only
        // makes the exhaustion worse — but the wait must not grow without
        // limit either, or a recovered daemon sits idle for hours.
        assert_eq!(
            tick_backoff_delay(1),
            DAEMON_LOOP_INTERVAL,
            "the first retry waits one interval"
        );
        assert!(
            tick_backoff_delay(3) > tick_backoff_delay(1),
            "successive failures must wait longer"
        );
        assert_eq!(
            tick_backoff_delay(1000),
            MAX_TICK_BACKOFF,
            "the wait must be capped, not unbounded"
        );

        // The wait must be cancellable. At the cap it is two minutes, and the
        // shutdown arm is what releases the daemon lock and stamps the
        // clean-exit timestamp — a SIGTERM held that long means the process is
        // killed first and the next boot reports an unclean exit that never
        // happened.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = tokio::time::Instant::now();
        let interrupted = backoff_after_failed_tick(1000, &cancel).await;
        assert!(interrupted, "a cancelled token must cut the wait short");
        assert!(
            start.elapsed() < MAX_TICK_BACKOFF,
            "shutdown must not wait out the full backoff"
        );
    }

    /// Guard: the liveness counter must not be bumped before the tick has
    /// reached a dispatch decision.
    ///
    /// This is the whole defect. Bumping at the top of the loop reports
    /// "alive" before anything has been attempted, so a scheduler that cannot
    /// dispatch a single task — descriptors exhausted, data dir read-only —
    /// keeps ticking, keeps looking healthy, and the watchdog that exists to
    /// restart it never fires. The property cannot be expressed in the type
    /// system: it is about *where* a statement sits relative to the failure
    /// paths, and the pause gate is the first point at which the tick has
    /// decided anything.
    #[test]
    fn liveness_is_never_bumped_before_the_tick_reaches_a_decision() {
        let src = include_str!("scheduler.rs");
        let body = src
            .split_once("_ = interval.tick() => {}")
            .expect("scheduler loop shape changed")
            .1;
        let first_decision = body
            .find("emergency_pause::is_engaged")
            .expect("pause gate not found");
        let first_bump = body
            .find("liveness.fetch_add")
            .expect("liveness bump not found");
        assert!(
            first_bump > first_decision,
            "the liveness bump must come after the tick has reached a dispatch \
             decision — bumping earlier hides a scheduler that dispatches nothing"
        );
    }

    // ── daemon-lock refresh policy ──

    #[test]
    fn transient_refresh_errors_are_tolerated_until_the_lock_could_go_stale() {
        // A busy timeout says nothing about who holds the lock, so the
        // dispatcher keeps going — but only while the row provably cannot
        // have gone stale. Past that, another daemon could take the lock, and
        // two dispatchers would run.
        let acquired = 10_000;
        let budget = LockRefreshPolicy::transient_budget_secs();
        assert!(
            budget > 0 && budget < DAEMON_LOCK_STALE_SECS,
            "the budget must end before the stale threshold, got {budget}"
        );
        let cases = [
            (acquired + 60, LockVerdict::Continue),
            (acquired + budget - 1, LockVerdict::Continue),
            (acquired + budget, LockVerdict::Cancel),
            (acquired + DAEMON_LOCK_STALE_SECS, LockVerdict::Cancel),
        ];
        // Fed through `from_result`, as the loop does: a refresh error of any
        // kind is the transient class.
        let transient = LockRefresh::from_result(&Err(anyhow::anyhow!("database is locked")));
        for (now, expected) in cases {
            let mut policy = LockRefreshPolicy::new(acquired);
            assert_eq!(
                policy.observe(transient, now),
                expected,
                "transient error at +{}s",
                now - acquired
            );
        }
    }

    #[test]
    fn a_successful_refresh_restarts_the_transient_budget() {
        let mut policy = LockRefreshPolicy::new(0);
        let budget = LockRefreshPolicy::transient_budget_secs();
        assert_eq!(
            policy.observe(LockRefresh::from_result(&Ok(true)), budget - 10),
            LockVerdict::Continue
        );
        // Without the refresh above this would be past the budget.
        assert_eq!(
            policy.observe(LockRefresh::Transient, budget + 10),
            LockVerdict::Continue,
            "the budget is measured from the last good refresh, not from acquire"
        );
        assert_eq!(
            policy.observe(LockRefresh::Transient, budget - 10 + budget),
            LockVerdict::Cancel
        );
    }

    #[test]
    fn a_lost_lock_cancels_at_once() {
        // 0 rows updated means another pid owns the row: there is no budget
        // to spend, the other daemon may already be dispatching.
        let mut policy = LockRefreshPolicy::new(0);
        assert_eq!(
            policy.observe(LockRefresh::from_result(&Ok(false)), 1),
            LockVerdict::Cancel
        );
    }

    // ── tick failure classes ──

    #[test]
    fn store_faults_a_restart_cannot_fix_are_healthy_skips() {
        // SQLITE_FULL = 13, SQLITE_BUSY = 5, SQLITE_LOCKED = 6; EMFILE = 24.
        let sqlite = |code: i32, msg: &str| -> anyhow::Error {
            libsql::Error::SqliteFailure(code, msg.to_string()).into()
        };
        let io = |errno: i32| -> anyhow::Error {
            anyhow::Error::new(std::io::Error::from_raw_os_error(errno))
                .context("opening tick lock file")
        };
        let cases: Vec<(&str, anyhow::Error, TickFailure)> = vec![
            (
                "SQLITE_FULL",
                sqlite(13, "database or disk is full"),
                TickFailure::HealthySkip(TickFailureClass::StorageExhausted),
            ),
            (
                "SQLITE_BUSY",
                sqlite(5, "database is locked"),
                TickFailure::HealthySkip(TickFailureClass::LockContention),
            ),
            (
                "SQLITE_LOCKED",
                sqlite(6, "database table is locked"),
                TickFailure::HealthySkip(TickFailureClass::LockContention),
            ),
            (
                "ENOSPC behind context",
                io(libc::ENOSPC),
                TickFailure::HealthySkip(TickFailureClass::StorageExhausted),
            ),
            ("EMFILE", io(libc::EMFILE), TickFailure::Unhealthy),
            (
                "unknown message",
                anyhow::anyhow!("no such table: task_runs"),
                TickFailure::Unhealthy,
            ),
        ];
        for (name, err, expected) in cases {
            assert_eq!(classify_tick_failure(&err), expected, "{name}");
        }
    }

    #[test]
    fn healthy_skip_warnings_repeat_once_per_class_per_interval() {
        let mut throttle = HealthySkipWarnThrottle::default();
        let t0 = Instant::now();
        assert!(throttle.should_warn(TickFailureClass::StorageExhausted, t0));
        assert!(
            !throttle.should_warn(
                TickFailureClass::StorageExhausted,
                t0 + Duration::from_secs(60)
            ),
            "the same class must stay quiet inside the interval"
        );
        assert!(
            throttle.should_warn(
                TickFailureClass::LockContention,
                t0 + Duration::from_secs(60)
            ),
            "a different class is throttled independently"
        );
        assert!(
            throttle.should_warn(
                TickFailureClass::StorageExhausted,
                t0 + HEALTHY_SKIP_WARN_INTERVAL
            ),
            "the class warns again once the interval has passed"
        );
    }

    #[test]
    fn a_healthy_skip_counts_as_alive_and_an_unhealthy_one_does_not() {
        let liveness = AtomicU64::new(0);
        let mut throttle = HealthySkipWarnThrottle::default();
        note_tick_failure(TickFailure::Unhealthy, &liveness, &mut throttle);
        assert_eq!(
            liveness.load(Ordering::Relaxed),
            0,
            "an unhealthy tick must leave the watchdog free to restart the loop"
        );
        note_tick_failure(
            TickFailure::HealthySkip(TickFailureClass::LockContention),
            &liveness,
            &mut throttle,
        );
        assert_eq!(liveness.load(Ordering::Relaxed), 1);
    }

    // ── boot recovery once per process ──

    #[test]
    fn boot_recovery_is_claimed_by_the_first_incarnation_only() {
        // No other test in this binary touches the claim, so the first call
        // here is the process's first.
        assert!(claim_boot_recovery(), "the first dispatcher runs recovery");
        assert!(
            !claim_boot_recovery(),
            "a respawned dispatcher must not fail the runs the first one spawned"
        );
    }

    /// Guard: the stalled-task scan must run after the claim.
    ///
    /// Before the claim, a store that refused claims for longer than the
    /// scan's grace window would have every due recurring task recorded as
    /// missed and re-armed the moment it recovered, instead of fired once.
    #[test]
    fn the_stalled_scan_runs_after_the_claim() {
        let src = include_str!("scheduler.rs");
        let body = src
            .split_once("_ = interval.tick() => {}")
            .expect("scheduler loop shape changed")
            .1;
        let claim = body.find("db.claim_due_tasks").expect("claim not found");
        let scan = body
            .find("sweep::stalled_scan")
            .expect("stalled scan not found");
        assert!(
            scan > claim,
            "the scan must come after the claim, or an outage turns due fires into missed ones"
        );
    }

    /// Guard: the stalled-task scan must run after the emergency-pause gate.
    ///
    /// `heal_stalled_tasks` re-arms overdue recurring tasks to their next
    /// occurrence and records the skipped fires as missed. Running it while
    /// paused turns the documented "deferred, not lost" promise into silent
    /// loss for any pause longer than the scan's grace window.
    #[test]
    fn the_stalled_scan_runs_after_the_pause_gate() {
        let src = include_str!("scheduler.rs");
        let body = src
            .split_once("_ = interval.tick() => {}")
            .expect("scheduler loop shape changed")
            .1;
        let pause_gate = body
            .find("emergency_pause::is_engaged")
            .expect("pause gate not found");
        let scan = body
            .find("sweep::stalled_scan")
            .expect("stalled scan not found");
        assert!(
            scan > pause_gate,
            "a paused daemon must heal nothing — the scan would re-arm the very \
             runs the pause promised to defer"
        );
    }
}
