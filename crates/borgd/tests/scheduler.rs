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
        let db = Database::open().expect("seed db");
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
        })
        .expect("create due-cmd");
    }

    // Spin up the dispatcher with the real task_runner hooks.
    let shutdown = CancellationToken::new();
    let semaphore = Arc::new(Semaphore::new(2));
    let hooks = scheduler::DispatcherHooks {
        spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
            Box::pin(async move {
                task_runner::spawn_task_execution(task, run_id, retry, sem, cfg, None).await
            })
        }),
        spawn_workflow_step: Arc::new(|wf, step, prior, all, sem, cfg| {
            Box::pin(async move {
                task_runner::spawn_workflow_step(wf, step, prior, all, sem, cfg, None).await
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

    // Poll for completion. The DAEMON_LOOP_INTERVAL is 1s in core::constants;
    // give the dispatcher a few ticks to claim + run + record.
    let mut completed = false;
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let db = Database::open().expect("db");
        let runs = db.task_run_history("due-cmd", 5).unwrap_or_default();
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
