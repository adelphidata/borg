//! Dispatcher honors the per-tick advisory lock.
//!
//! Real failure mode: the `TickLock` acquire is wired in the wrong place
//! (e.g. after `claim_due_tasks` instead of before), or the `continue` arm
//! is missing on collision, or `drop(lock)` is forgotten — and the
//! dispatcher either claims tasks while another holder owns the lock
//! (overlap) or self-deadlocks after the first iteration (stops claiming).
//!
//! This test pins the observable contract: while an external holder owns
//! the tick lock, the dispatcher MUST NOT claim due tasks, regardless of
//! how many `interval.tick()` firings happen during that window.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::paths::tick_lock_path;
use borgd::tick_lock::TickLock;
use borgd::{scheduler, task_runner};
use cortex::config::Config;
use cortex::db::{Database, NewTask};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Serializes tests in this binary because they mutate the process-wide
/// `BORG_DATA_DIR` env var.
static SERIAL: Mutex<()> = Mutex::new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_skips_claim_while_external_holder_owns_tick_lock() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary serialize via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }

    // Seed a due command task (POSIX no-op) — would succeed in ~milliseconds
    // if the dispatcher ever claimed it.
    {
        let db = Database::open().expect("seed db");
        let now = chrono::Utc::now().timestamp();
        db.create_task(&NewTask {
            id: "blocked-by-tick-lock",
            name: "blocked-by-tick-lock",
            prompt: ":",
            schedule_type: "once",
            schedule_expr: "",
            timezone: "UTC",
            next_run: Some(now - 1),
            max_retries: Some(0),
            timeout_ms: Some(5_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "command",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .expect("create task");
    }

    // Hold the tick lock from the test's perspective BEFORE starting the
    // dispatcher. The dispatcher's first `interval.tick()` returns
    // immediately, so it will hit the locked path on its very first
    // iteration. If our skip-on-collision arm regresses, the task will
    // already be claimed by the time we assert.
    let lock_path = tick_lock_path(&Config::data_dir().expect("data_dir"));
    let _external_lock = TickLock::try_acquire(&lock_path)
        .expect("test holder acquire")
        .expect("test holder must own the lock");

    let shutdown = CancellationToken::new();
    let semaphore = Arc::new(Semaphore::new(2));
    let hooks = scheduler::DispatcherHooks {
        spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
            Box::pin(async move {
                task_runner::spawn_task_execution(task, run_id, retry, sem, cfg, None).await
            })
        }),
        deliver_celebration: Arc::new(|cfg, c| {
            Box::pin(async move { task_runner::deliver_celebration_to_channels(cfg, c).await })
        }),
    };
    let cfg = Config::default();
    let shutdown_for_run = shutdown.clone();
    let dispatcher =
        tokio::spawn(async move { scheduler::run(cfg, hooks, semaphore, shutdown_for_run).await });

    // Give the dispatcher time to spin up, hit its first immediate tick,
    // and run through the lock-collision path. 2s is comfortably more than
    // boot-recovery + one iteration through the loop body.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify no claim happened while we held the lock.
    let runs_during_hold = {
        let db = Database::open().expect("db");
        db.task_run_history("blocked-by-tick-lock", 5)
            .unwrap_or_default()
    };
    assert!(
        runs_during_hold.is_empty(),
        "dispatcher must not record any task_runs while the tick lock is externally held; \
         got {} run(s): {:?}",
        runs_during_hold.len(),
        runs_during_hold
            .iter()
            .map(|r| r.status.as_str())
            .collect::<Vec<_>>()
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;
    drop(_external_lock);

    // SAFETY: serialized via SERIAL.
    unsafe {
        std::env::remove_var("BORG_DATA_DIR");
    }
}
