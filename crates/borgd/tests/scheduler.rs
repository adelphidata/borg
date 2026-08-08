//! Scheduled-task dispatcher smoke test.
//!
//! Real failure mode: the in-borgd `scheduler::run` loop never picks up a due
//! task — either because the dispatcher wasn't lifted from `service::run_daemon`,
//! or because `claim_due_tasks` isn't being called, or because the task spawn
//! callback got mis-wired and never invokes `task_runner::spawn_task_execution`.
//!
//! We seed a `command` task whose body is `:` (the POSIX no-op), set
//! `next_run = now - 1s` so the dispatcher claims it on the first tick,
//! then poll the DB for a `task_runs` row marked `success`. A regression in
//! the dispatcher -> task_runner wiring would leave `task_runs` empty.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use borgd::scheduler;
use borgd::task_runner;
use cortex::config::Config;
use cortex::db::{Database, NewTask};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

static SERIAL: Mutex<()> = Mutex::new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_claims_due_command_task_and_records_task_run() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary serialize via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }

    // Seed a due command task.
    {
        let db = Database::open().await.expect("seed db");
        let now = chrono::Utc::now().timestamp();
        db.create_task(&NewTask {
            id: "due-cmd",
            name: "due-cmd",
            prompt: ":", // POSIX no-op — succeeds with no output, no LLM needed
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
        .await
        .expect("create due-cmd");
    }

    // Spin up the dispatcher with the real task_runner hooks.
    let shutdown = CancellationToken::new();
    let semaphore = Arc::new(Semaphore::new(2));
    let hooks = scheduler::DispatcherHooks {
        spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
            Box::pin(async move {
                task_runner::spawn_task_execution(task, run_id, retry, sem, cfg, None, None).await
            })
        }),
        deliver_celebration: Arc::new(|cfg, c| {
            Box::pin(async move { task_runner::deliver_celebration_to_channels(cfg, c).await })
        }),
    };
    let cfg = Config::default();
    let shutdown_for_run = shutdown.clone();
    let dispatcher = tokio::spawn(async move {
        // In production the supervisor supplies this counter and watches it
        // for stalls; driving `run` directly, a local one suffices.
        let liveness = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
    });

    // Poll for completion. The DAEMON_LOOP_INTERVAL is 1s in core::constants;
    // give the dispatcher a few ticks to claim + run + record.
    let mut completed = false;
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let db = Database::open().await.expect("db");
        let runs = db.task_run_history("due-cmd", 5).await.unwrap_or_default();
        if runs.iter().any(|r| r.status == "success") {
            completed = true;
            break;
        }
    }

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;

    assert!(
        completed,
        "dispatcher never recorded a successful task_runs row for the due command task"
    );
}

/// Emergency-stop gate: while `security.emergency_paused` is set, the
/// dispatcher must not claim due tasks — and because it never claims,
/// `next_run` stays untouched, so the run is deferred rather than lost.
/// After resume, a dispatcher tick claims and executes the same task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emergency_pause_defers_dispatch_and_resume_restores_it() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests in this binary serialize via SERIAL.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }

    let due_at;
    {
        let db = Database::open().await.expect("seed db");
        let now = chrono::Utc::now().timestamp();
        due_at = now - 1;
        db.create_task(&NewTask {
            id: "paused-cmd",
            name: "paused-cmd",
            prompt: ":",
            schedule_type: "once",
            schedule_expr: "",
            timezone: "UTC",
            next_run: Some(due_at),
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
        .await
        .expect("create paused-cmd");
        db.set_setting(cortex::emergency_pause::SETTING_KEY, "true")
            .await
            .expect("engage pause");
    }

    let hooks = || scheduler::DispatcherHooks {
        spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
            Box::pin(async move {
                task_runner::spawn_task_execution(task, run_id, retry, sem, cfg, None, None).await
            })
        }),
        deliver_celebration: Arc::new(|cfg, c| {
            Box::pin(async move { task_runner::deliver_celebration_to_channels(cfg, c).await })
        }),
    };

    // Phase 1: paused. The dispatcher's first tick fires immediately; give it
    // a couple of seconds and assert it neither ran the task nor consumed the
    // due `next_run` (deferred, not lost).
    {
        let shutdown = CancellationToken::new();
        let semaphore = Arc::new(Semaphore::new(2));
        let hooks = hooks();
        let cfg = Config::default();
        let shutdown_for_run = shutdown.clone();
        let dispatcher = tokio::spawn(async move {
            let liveness = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;

        let db = Database::open().await.expect("db");
        let runs = db
            .task_run_history("paused-cmd", 5)
            .await
            .unwrap_or_default();
        assert!(
            runs.is_empty(),
            "paused dispatcher must not claim/run the due task, got {} run(s)",
            runs.len()
        );
        let task = db
            .get_task_by_id("paused-cmd")
            .await
            .expect("get task")
            .expect("task row");
        assert_eq!(
            task.next_run,
            Some(due_at),
            "pause must defer the run: next_run may not be consumed or advanced"
        );
    }

    // Phase 2: resume. A fresh dispatcher (immediate first tick) must claim
    // and complete the very same deferred run.
    {
        let db = Database::open().await.expect("db");
        db.set_setting(cortex::emergency_pause::SETTING_KEY, "false")
            .await
            .expect("release pause");
    }
    let shutdown = CancellationToken::new();
    let semaphore = Arc::new(Semaphore::new(2));
    let hooks = hooks();
    let cfg = Config::default();
    let shutdown_for_run = shutdown.clone();
    let dispatcher = tokio::spawn(async move {
        let liveness = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
    });

    let mut completed = false;
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let db = Database::open().await.expect("db");
        let runs = db
            .task_run_history("paused-cmd", 5)
            .await
            .unwrap_or_default();
        if runs.iter().any(|r| r.status == "success") {
            completed = true;
            break;
        }
    }

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;

    assert!(
        completed,
        "after resume the deferred task must run to success"
    );
}
