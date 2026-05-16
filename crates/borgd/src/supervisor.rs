//! Generic subsystem supervisor for `borgd`'s long-running tasks.
//!
//! Wraps `tokio::spawn` with three properties:
//!
//! 1. **Panic catching.** A panic that escapes the task is captured
//!    via `JoinHandle.await` returning `JoinError::Panic`. The
//!    supervisor logs the payload and respawns instead of letting
//!    Tokio's default panic-swallowing turn the daemon into a
//!    half-working zombie.
//!
//! 2. **Result-error treatment.** A task returning `Err(_)` is
//!    treated identically to a panic — both mean "this subsystem
//!    needs to be restarted." This matches how operators think about
//!    the system: scheduler dispatcher exited unexpectedly is
//!    scheduler dispatcher exited unexpectedly, regardless of the
//!    Rust mechanism.
//!
//! 3. **Degraded-state cap.** After more than `max_restarts` restarts
//!    inside `restart_window`, the supervisor stops respawning, logs
//!    an error, and surfaces the subsystem in `maintenance_issue_state`
//!    so the next maintenance sweep promotes it to a doctor `Fail`.
//!    The alternative — respawning forever — turns a stable bug into
//!    a quiet CPU drain that's invisible until someone reads logs.
//!
//! Cancellation is honored on every await. A cancellation through the
//! shared `CancellationToken` returns cleanly without burning a restart.

use cortex::db::Database;
use cortex::util::panic_utils::panic_msg;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

/// `Database` is now `Send + Sync` (turso connection is internally `Arc`'d),
/// so subsystems share access through a plain `Arc`. The legacy
/// `Arc<Mutex<Database>>` wrapper became dead weight once the SQL layer
/// went async.
pub type DbHandle = Arc<Database>;

async fn try_log(db: &DbHandle, level: &str, category: &str, message: &str) {
    if let Err(e) = db.log_activity(level, category, message, None).await {
        tracing::debug!("supervisor: activity log write failed: {e}");
    }
}

async fn try_observe_issue(db: &DbHandle, key: &str, ts: i64, status: &str) {
    if let Err(e) = db.upsert_issue_state_observed(key, ts, status).await {
        tracing::debug!("supervisor: issue-state observe failed: {e}");
    }
}

/// Knobs for one supervised subsystem. Every field has a default
/// suitable for borgd's long-running daemon tasks; override only
/// when there's a concrete reason (e.g., the gateway's historic
/// 5-restarts-with-exp-backoff that pre-dated this module).
pub struct SupervisorConfig {
    /// Subsystem name surfaced in logs, activity_log rows, and the
    /// `maintenance_issue_state` key. Stable across daemon restarts —
    /// renaming changes the doctor finding identity.
    pub name: &'static str,
    /// Restarts within `restart_window` past which the supervisor
    /// gives up.
    pub max_restarts: u32,
    /// Sliding window for counting restarts. A subsystem that crashes
    /// once a day forever is fine; one that crashes 5 times in 10
    /// minutes is not.
    pub restart_window: Duration,
    /// First respawn delay; doubled each restart up to `max_backoff`.
    pub base_backoff: Duration,
    /// Cap on respawn delay so an exponentially-backed-off subsystem
    /// doesn't take an hour to retry.
    pub max_backoff: Duration,
    /// Optional callback fired exactly once when the subsystem enters
    /// degraded state (max_restarts exceeded). Used by the gateway to
    /// stamp the legacy `gateway_unrecoverable` meta keys the
    /// heartbeat augmenter reads. Errors inside the callback are
    /// caught and logged so a panicking callback can't mask the
    /// degradation event itself.
    pub on_degraded: Option<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for SupervisorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorConfig")
            .field("name", &self.name)
            .field("max_restarts", &self.max_restarts)
            .field("restart_window", &self.restart_window)
            .field("base_backoff", &self.base_backoff)
            .field("max_backoff", &self.max_backoff)
            .field("on_degraded", &self.on_degraded.is_some())
            .finish()
    }
}

impl SupervisorConfig {
    /// Construct a config with defaults — most call sites only need
    /// to override `name`.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            max_restarts: 5,
            restart_window: Duration::from_secs(600),
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            on_degraded: None,
        }
    }

    /// Builder: install a callback fired when the subsystem hits
    /// degraded state. Replaces any previously set callback.
    pub fn with_on_degraded<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_degraded = Some(Box::new(f));
        self
    }
}

/// Outcome of one supervised iteration. Surfaced via logs and the
/// `subsystem` activity-log category so operators can audit restart
/// history without stepping through stack traces.
#[derive(Debug)]
enum IterationOutcome {
    /// Task completed cleanly (returned `Ok(())`). The supervisor
    /// stops — there's nothing left to supervise.
    CleanExit,
    /// Cancellation token fired. Supervisor returns without counting
    /// against the restart budget.
    Cancelled,
    /// Task returned `Err(_)`. Counts as a restart.
    TaskError(String),
    /// Task panicked. Counts as a restart.
    Panicked(String),
    /// Task was aborted from elsewhere (e.g., the heartbeat watchdog
    /// firing `JoinHandle::abort` on a stalled scheduler). Counts as
    /// a restart so persistent stalls escalate to degraded state.
    Aborted,
}

/// Spawn a supervised task. The `factory` closure is called once per
/// (re)spawn and must return the future to run. Returns a `JoinHandle`
/// for the supervisor itself — `await` it during shutdown to ensure
/// the supervised task drains.
///
/// The factory is `Fn` (not `FnOnce`) because the supervisor calls it
/// each respawn. Capture state via `Arc::clone` inside the closure if
/// needed.
pub fn spawn_supervised<F, Fut, E>(
    cfg: SupervisorConfig,
    cancel: CancellationToken,
    db: DbHandle,
    factory: F,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    tokio::spawn(supervised_loop(cfg, cancel, db, factory))
}

async fn supervised_loop<F, Fut, E>(
    cfg: SupervisorConfig,
    cancel: CancellationToken,
    db: DbHandle,
    factory: F,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    // (Instant, _) tuples form the sliding window of recent restart
    // events. Pruned each iteration so old restarts stop counting.
    let mut recent_restarts: Vec<Instant> = Vec::new();

    loop {
        if cancel.is_cancelled() {
            return;
        }

        let future = factory();
        let join = tokio::spawn(AssertUnwindSafe(future).catch_unwind());
        let abort = join.abort_handle();

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Best-effort cancel of the inner task — the cancel
                // token would normally have signalled it through, but
                // an inner task that doesn't observe the token still
                // gets aborted at shutdown.
                abort.abort();
                IterationOutcome::Cancelled
            }
            res = join => match res {
                // Inner future returned cleanly with Ok.
                Ok(Ok(Ok(()))) => IterationOutcome::CleanExit,
                // Inner future returned Err — task-level error, treat as restart.
                Ok(Ok(Err(e))) => IterationOutcome::TaskError(e.to_string()),
                // Inner future panicked.
                Ok(Err(payload)) => IterationOutcome::Panicked(panic_msg(&payload)),
                // The spawn was cancelled (someone called join.abort()
                // via an external AbortHandle, e.g. the watchdog).
                Err(je) if je.is_cancelled() => {
                    if cancel.is_cancelled() {
                        IterationOutcome::Cancelled
                    } else {
                        IterationOutcome::Aborted
                    }
                }
                Err(je) => IterationOutcome::Panicked(je.to_string()),
            }
        };

        match outcome {
            IterationOutcome::CleanExit => {
                // Predictable startup/shutdown event — fires per subsystem
                // (6+ on every daemon start). Operators don't need it at INFO.
                tracing::debug!(subsystem = %cfg.name, "exited cleanly");
                return;
            }
            IterationOutcome::Cancelled => {
                tracing::debug!(subsystem = %cfg.name, "cancelled, shutting down");
                return;
            }
            IterationOutcome::TaskError(msg) => {
                tracing::warn!("subsystem {}: returned Err: {msg}", cfg.name);
                try_log(
                    &db,
                    "warn",
                    "subsystem",
                    &format!("{}: returned Err: {msg}", cfg.name),
                )
                .await;
            }
            IterationOutcome::Panicked(msg) => {
                tracing::error!("subsystem {}: panicked: {msg}", cfg.name);
                try_log(
                    &db,
                    "error",
                    "subsystem",
                    &format!("{}: panicked: {msg}", cfg.name),
                )
                .await;
            }
            IterationOutcome::Aborted => {
                tracing::warn!(
                    "subsystem {}: externally aborted (likely watchdog)",
                    cfg.name
                );
                try_log(
                    &db,
                    "warn",
                    "subsystem",
                    &format!(
                        "{}: externally aborted (likely watchdog stall recovery)",
                        cfg.name
                    ),
                )
                .await;
            }
        }

        let now = Instant::now();
        recent_restarts.retain(|t| now.duration_since(*t) < cfg.restart_window);
        recent_restarts.push(now);

        if recent_restarts.len() as u32 > cfg.max_restarts {
            tracing::error!(
                subsystem = %cfg.name,
                restart_count = recent_restarts.len(),
                window_secs = cfg.restart_window.as_secs(),
                max_restarts = cfg.max_restarts,
                "subsystem entering degraded state"
            );
            let key = format!("Subsystem:{}", cfg.name);
            let unix_now = chrono::Utc::now().timestamp();
            try_log(
                &db,
                "error",
                "subsystem",
                &format!(
                    "{}: degraded after {} restarts in {:?}; supervisor giving up",
                    cfg.name,
                    recent_restarts.len(),
                    cfg.restart_window,
                ),
            )
            .await;
            try_observe_issue(&db, &key, unix_now, "fail").await;
            if let Some(cb) = cfg.on_degraded.as_ref() {
                // The callback runs sync inside the supervisor task. A
                // panic here must NOT prevent the degraded-state log /
                // issue_state writes above (which already happened) and
                // must not crash the supervisor itself. catch_unwind it.
                let cb_ref: &(dyn Fn() + Send + Sync) = cb.as_ref();
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cb_ref))
                {
                    let msg = panic_msg(&payload);
                    tracing::error!(
                        "subsystem {}: on_degraded callback panicked: {msg}",
                        cfg.name
                    );
                }
            }
            return;
        }

        // Exponential backoff capped at `max_backoff`. Counter is the
        // window length, not total restarts ever, so a subsystem that
        // recovers and stays healthy for a while gets a fresh budget.
        let exp = recent_restarts.len().min(8) as u32;
        let raw_backoff = cfg
            .base_backoff
            .saturating_mul(1u32 << exp.saturating_sub(1));
        let backoff = raw_backoff.min(cfg.max_backoff);

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

/// Polling interval for the liveness watchdog. 30s × 10 polls = 5min
/// stale threshold, with 10× headroom over the heartbeat scheduler's
/// 60s liveness tick floor.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Number of consecutive unchanged-counter polls that trigger an
/// abort. 10 × 30s poll = 5min stall threshold. Conservative — the
/// scheduler's LIVENESS_TICK_MAX is 60s, so a healthy loop touches its
/// counter every minute. Five minutes silent = wedged.
const WATCHDOG_MAX_STALE_POLLS: u32 = 10;

/// Spawn a liveness watchdog. Periodically reads `liveness`; if its
/// value doesn't change for [`WATCHDOG_MAX_STALE_POLLS`] consecutive
/// polls, calls `abort.abort()` so the supervisor's restart logic
/// kicks in.
///
/// The watchdog is itself supervised by the surrounding subsystem
/// supervisor — a panic inside the watchdog would otherwise leave the
/// scheduler unwatched. In practice the body is so simple (atomic
/// load, sleep, compare) that a panic is almost impossible, but the
/// supervision is cheap and removes one class of "unmonitored"
/// failure.
///
/// Returns the spawned task's `JoinHandle` so callers can await it on
/// shutdown if they want clean drain semantics.
pub fn spawn_liveness_watchdog(
    name: &'static str,
    liveness: Arc<AtomicU64>,
    abort: AbortHandle,
    cancel: CancellationToken,
    db: DbHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = liveness.load(Ordering::Relaxed);
        let mut stale_polls: u32 = 0;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(WATCHDOG_POLL_INTERVAL) => {}
                _ = cancel.cancelled() => return,
            }
            let now_val = liveness.load(Ordering::Relaxed);
            if now_val == last {
                stale_polls = stale_polls.saturating_add(1);
                if stale_polls >= WATCHDOG_MAX_STALE_POLLS {
                    let stall_secs = WATCHDOG_POLL_INTERVAL.as_secs() * stale_polls as u64;
                    tracing::warn!(
                        "watchdog: subsystem {name} stalled for ~{stall_secs}s (counter={now_val} unchanged); aborting for restart"
                    );
                    try_log(
                        &db,
                        "warn",
                        "subsystem",
                        &format!(
                            "{name}: watchdog detected stall (~{stall_secs}s, counter={now_val}); restart triggered"
                        ),
                    )
                    .await;
                    abort.abort();
                    return;
                }
            } else {
                // Counter advanced — healthy. Reset the stale window.
                stale_polls = 0;
                last = now_val;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn test_db() -> DbHandle {
        // The DbHandle is now a plain `Arc<Database>` — turso's connection is
        // internally Arc'd, so the Mutex wrapper that pre-dated the async DB
        // is no longer needed.
        Arc::new(Database::test_db().await)
    }

    /// Convenience: query activity rows through the `DbHandle`.
    async fn read_activity(
        db: &DbHandle,
        level: Option<&str>,
        category: Option<&str>,
    ) -> Vec<cortex::db::ActivityEntry> {
        db.query_activity(50, level, category).await.unwrap()
    }

    fn fast_cfg(name: &'static str, max_restarts: u32) -> SupervisorConfig {
        SupervisorConfig {
            name,
            max_restarts,
            restart_window: Duration::from_secs(60),
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
            on_degraded: None,
        }
    }

    #[tokio::test]
    async fn respawns_after_panic_then_succeeds() {
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let handle = spawn_supervised(
            fast_cfg("respawn-test", 5),
            cancel.clone(),
            db.clone(),
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        panic!("transient bug attempt {n}");
                    }
                    Ok::<(), anyhow::Error>(())
                }
            },
        );

        // The task ultimately completes after 2 panics + 1 success.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor stuck")
            .expect("supervisor task panicked");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "should have attempted 3 times (2 panics + 1 success)"
        );

        // Two panic restart rows (the 3rd attempt succeeded so wrote no row).
        // `error`-level rows count too — panics are logged at error.
        let rows = read_activity(&db, Some("warn"), Some("subsystem")).await;
        let panic_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.message.contains("panicked"))
            .collect();
        assert_eq!(
            panic_rows.len(),
            2,
            "expected 2 panic activity rows, got {}",
            panic_rows.len()
        );
    }

    #[tokio::test]
    async fn enters_degraded_state_after_max_restarts() {
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let handle = spawn_supervised(
            fast_cfg("degrade-test", 3),
            cancel.clone(),
            db.clone(),
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(anyhow::anyhow!("always fails"))
                }
            },
        );

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor stuck")
            .expect("supervisor task panicked");

        // After max_restarts=3, we tried 4 times (the count exceeds max,
        // not equals — so 4th is the trigger).
        assert_eq!(attempts.load(Ordering::SeqCst), 4);

        // One degraded-state error row.
        let error_rows = read_activity(&db, Some("error"), Some("subsystem")).await;
        let degrade_rows: Vec<_> = error_rows
            .iter()
            .filter(|r| r.message.contains("degraded"))
            .collect();
        assert_eq!(
            degrade_rows.len(),
            1,
            "expected exactly one degraded-state row, got {}",
            degrade_rows.len()
        );

        // Issue state row exists with status fail.
        let row: Option<(String, String)> = {
            let mut stmt = db
                .conn()
                .prepare(
                    "SELECT issue_key, last_status FROM maintenance_issue_state WHERE issue_key = ?1",
                )
                .await
                .expect("prepare issue_state lookup");
            let mut rows = stmt
                .query([turso::Value::from("Subsystem:degrade-test")])
                .await
                .expect("query issue_state");
            rows.next().await.expect("fetch row").map(|r| {
                (
                    r.get::<String>(0).expect("issue_key"),
                    r.get::<String>(1).expect("last_status"),
                )
            })
        };
        let row = row.expect("issue_state row must exist for degraded subsystem");
        assert_eq!(row.0, "Subsystem:degrade-test");
        assert_eq!(row.1, "fail");
    }

    #[tokio::test]
    async fn cancellation_returns_without_counting_restart() {
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = spawn_supervised(
            fast_cfg("cancel-test", 5),
            cancel.clone(),
            db.clone(),
            move || async move {
                // Long-running task that respects cancellation.
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<(), anyhow::Error>(())
            },
        );

        // Cancel after a brief delay.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor did not respect cancel")
            .expect("supervisor task panicked");

        // No restart rows.
        let rows = read_activity(&db, Some("warn"), Some("subsystem")).await;
        let restart_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.message.contains("panicked") || r.message.contains("returned Err"))
            .collect();
        assert!(
            restart_rows.is_empty(),
            "cancel must not produce restart rows, got {restart_rows:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_aborts_when_counter_stuck() {
        // A liveness counter that never moves must trigger an abort
        // after WATCHDOG_MAX_STALE_POLLS polls.
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let liveness = Arc::new(AtomicU64::new(42));

        // Spawn a victim task that just sleeps forever — the watchdog
        // is the only thing that can cancel it.
        let victim = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        let abort = victim.abort_handle();

        let _wd = spawn_liveness_watchdog(
            "stall-test",
            liveness.clone(),
            abort,
            cancel.clone(),
            db.clone(),
        );

        // Advance enough to cross the stale threshold.
        let total = WATCHDOG_POLL_INTERVAL * (WATCHDOG_MAX_STALE_POLLS + 1);
        tokio::time::advance(total).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        // Victim must have been aborted by the watchdog.
        let err = victim
            .await
            .expect_err("watchdog should have aborted the victim");
        assert!(
            err.is_cancelled(),
            "abort must surface as cancelled JoinError, got: {err:?}"
        );

        // Watchdog logged a warn row.
        let rows = read_activity(&db, Some("warn"), Some("subsystem")).await;
        let stall_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.message.contains("watchdog detected stall"))
            .collect();
        assert_eq!(
            stall_rows.len(),
            1,
            "expected exactly one stall row, got {}",
            stall_rows.len()
        );

        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_does_not_abort_when_counter_advances() {
        // A counter that advances on every poll must NOT trigger an
        // abort, even after many polling intervals.
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let liveness = Arc::new(AtomicU64::new(0));

        let victim = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        let abort = victim.abort_handle();

        // Spawn a "healthy iterator" that bumps liveness every 10s.
        let liveness_writer = liveness.clone();
        let writer_cancel = cancel.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        liveness_writer.fetch_add(1, Ordering::Relaxed);
                    }
                    _ = writer_cancel.cancelled() => return,
                }
            }
        });

        let _wd =
            spawn_liveness_watchdog("healthy-test", liveness, abort, cancel.clone(), db.clone());

        // Advance well past the stall threshold's worth of virtual time.
        // The healthy writer is bumping liveness within each poll, so
        // the watchdog must keep finding a fresh value.
        let total = WATCHDOG_POLL_INTERVAL * (WATCHDOG_MAX_STALE_POLLS * 3);
        tokio::time::advance(total).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        // Victim must still be running (not aborted).
        assert!(
            !victim.is_finished(),
            "victim must not be aborted while liveness advances"
        );

        // No stall rows logged.
        let rows = read_activity(&db, Some("warn"), Some("subsystem")).await;
        let stall_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.message.contains("watchdog detected stall"))
            .collect();
        assert!(
            stall_rows.is_empty(),
            "no stall rows should be logged on healthy liveness, got {stall_rows:?}"
        );

        cancel.cancel();
        let _ = writer.await;
        let _ = victim.await;
    }

    #[tokio::test]
    async fn clean_exit_does_not_respawn() {
        let db = test_db().await;
        let cancel = CancellationToken::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let handle = spawn_supervised(
            fast_cfg("clean-test", 5),
            cancel.clone(),
            db.clone(),
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                }
            },
        );

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor stuck")
            .expect("supervisor task panicked");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "Ok(()) is a clean exit — supervisor must not respawn it"
        );
    }
}
