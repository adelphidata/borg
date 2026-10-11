//! Integration test: a scheduled command's output is redacted before it is
//! persisted or delivered.
//!
//! Spec invariant (AGENTS.md → "Prompt Injection", layer 5 "secret
//! redaction"): secrets must not survive into anything Borg stores or sends.
//!
//! The scheduled-task lane is the one place that had no coverage for this. Its
//! `pre_run` capture was redacted, but the task's own stdout/stderr went raw
//! into `task_runs.result` and into `deliver_task_result`, which calls the
//! Telegram/Slack/Discord API clients directly — so it never passes the
//! gateway's `channel_trait.rs` redaction chokepoint that covers interactive
//! replies. A cron job that printed a token handed the raw value to a chat
//! channel and to the DB.
//!
//! This drives the real `scheduler::run` loop against a real `scheduled_tasks`
//! row whose command echoes a secret-shaped token, then reads back the
//! persisted `task_runs.result`. `task_runs.result` is the same string handed
//! to `deliver_task_result`, so asserting on the stored row covers both
//! consumers.
//!
//! ── Mutation verification ──
//!
//! • In `task_runner::execute_command_task`, delete the
//!   `let combined = cortex::output_redaction::current_policy().apply(...)`
//!   rebinding that follows the `stdout`/`stderr` join: this test fails
//!   because the raw token body appears in `task_runs.result`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::uninlined_format_args,
    clippy::await_holding_lock
)]

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use borgd::{scheduler, task_runner};
use cortex::config::Config;
use cortex::db::{Database, NewTask};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Shares the process-global `BORG_HOME` env var with every other test in
/// this binary, so runs are serialized against a clean tempdir.
static SERIAL: Mutex<()> = Mutex::new(());

/// A GitHub-token-shaped string: `ghp_` + 36 chars, which is what the
/// `(?:ghp|ghs|gho|ghu|ghm)_[A-Za-z0-9_]{36,}` pattern matches. Built from a
/// literal so the assertion below is checking the redactor's behavior, not a
/// value this test computed.
const FAKE_TOKEN: &str = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

/// Distinctive body of the token — the part that must not survive. Asserting
/// on this rather than the whole string means a redactor that masked only the
/// `ghp_` prefix would still fail.
const TOKEN_BODY: &str = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: env mutation can race FFI `getenv` under a multi-threaded test
    // runner. SERIAL guarantees no other test in this binary touches
    // BORG_HOME concurrently.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }
    tmp
}

/// Insert a `task_type=command` row, due now, whose command prints the token
/// on stdout and again on stderr — both streams are folded into the same
/// `combined` string, so both must come back masked.
async fn seed_due_command_task(id: &str) {
    let db = Database::open().await.expect("open db");
    let now = chrono::Utc::now().timestamp();
    db.create_task(&NewTask {
        id,
        name: id,
        prompt: &format!("echo 'token {FAKE_TOKEN}'; echo 'token {FAKE_TOKEN}' >&2"),
        schedule_type: "once",
        schedule_expr: "",
        timezone: "UTC",
        next_run: Some(now - 1),
        max_retries: Some(0),
        timeout_ms: Some(15_000),
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "command",
        pre_run_command: None,
        pre_run_cwd: None,
        pre_run_timeout_ms: None,
        min_interval_secs: None,
        continuity: false,
    })
    .await
    .expect("create due command task");
}

/// Run the dispatcher until `task_id` has a finished `task_runs` row, then
/// return that row's `output`.
async fn run_and_read_output(config: &Config, task_id: &str) -> Option<String> {
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
        shutdown_process: Arc::new(|| {}),
    };
    let cfg = config.clone();
    let shutdown_for_run = shutdown.clone();
    let handle = tokio::spawn(async move {
        // Liveness counter is the supervisor's stall signal in production;
        // this test drives `run` directly, so a local counter is all it needs.
        let liveness = Arc::new(AtomicU64::new(0));
        scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut output = None;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Ok(db) = Database::open().await else {
            continue;
        };
        let Ok(mut rows) = db
            .conn()
            .query(
                "SELECT COALESCE(result, ''), COALESCE(error, ''), status \
                 FROM task_runs WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
                libsql::params![task_id],
            )
            .await
        else {
            continue;
        };
        if let Ok(Some(row)) = rows.next().await {
            let out = row.get::<String>(0).unwrap_or_default();
            let err = row.get::<String>(1).unwrap_or_default();
            let status = row.get::<String>(2).unwrap_or_default();
            if status != "running" {
                assert!(
                    err.is_empty(),
                    "command task must succeed for this test to mean anything, \
                     but it errored: {err} (status={status})"
                );
                output = Some(out);
                break;
            }
        }
    }

    shutdown.cancel();
    let _ = handle.await;
    output
}

#[tokio::test]
async fn scheduled_command_output_is_redacted_before_it_is_persisted() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let mut config = Config::default();
    // A command task never calls the LLM, but the dispatcher still builds the
    // surrounding agent context. Keep the heavyweight side systems quiet so
    // the test measures the redaction and nothing else.
    config.memory.active_injection.enabled = false;
    config.memory.embeddings.enabled = false;

    seed_due_command_task("redaction-probe").await;
    let output = run_and_read_output(&config, "redaction-probe").await;

    let output = output.expect(
        "the dispatcher must record an output row for the command task \
         within the deadline — without one this test asserts nothing",
    );

    // Precondition: the command really ran and its output was captured. A
    // task that failed to spawn would leave an empty string that trivially
    // contains no token.
    assert!(
        output.contains("token"),
        "precondition: captured output must contain the command's own text, \
         got {output:?}"
    );

    assert!(
        !output.contains(TOKEN_BODY),
        "the token body survived into task_runs.result — this string is also \
         what deliver_task_result sends to a chat channel. Got: {output:?}"
    );
}
