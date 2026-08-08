//! Integration test: scheduled-task `pre_run_command` injection end-to-end.
//!
//! Spec invariants (CLAUDE.md → Tools, `schedule.create`):
//!   - For a `task_type=prompt` task with a `pre_run_command`, stdout is
//!     injected into the LLM prompt as a
//!     `<preprocessed_context source="pre_run" exit_code="N">` block ahead
//!     of the user prompt.
//!   - Non-zero exit / timeout / spawn failure surface as `status="failed"`
//!     and the LLM still runs — silent preamble failures lose user trust.
//!   - Captured stdout/stderr cap at 64 KiB.
//!
//! Unit tests in `crates/borgd/src/task_runner.rs::pre_run_tests` already
//! cover the `run_pre_run_command` helper (cwd, exit-code surfacing, timeout,
//! truncation, XML escaping). This test plugs the helper into the full
//! dispatcher path: a real `scheduled_tasks` row, the real `scheduler::run`
//! loop, the real `task_runner::spawn_task_execution`, and a real LLM HTTP
//! call against a wiremock that captures the prompt body. If any of the
//! wiring between those pieces breaks (the preamble runs but its output
//! never gets into the prompt; failure short-circuits before the LLM call;
//! the dispatcher swallows the row), the assertions on the captured request
//! body — not just the helper's return value — will fail.
//!
//! ── Mutation verification ──
//!
//! • In `execute_prompt_task`, replace `format!("{}{task_prompt}",
//!   render_pre_run_block(&outcome))` with `task_prompt.clone()` (drop the
//!   block):  `success_preamble_injects_preprocessed_context_block` fails
//!   because the captured body no longer contains the
//!   `<preprocessed_context>` open tag.
//! • In `execute_prompt_task`, wrap the LLM call inside
//!   `if outcome.success { ... }` so a failure preamble skips the LLM:
//!   `failure_preamble_still_invokes_llm_with_failed_status` fails because
//!   wiremock never receives a request.

#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used,
    clippy::uninlined_format_args,
    clippy::await_holding_lock
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use borgd::{scheduler, task_runner};
use cortex::config::Config;
use cortex::db::{Database, NewTask};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Tests in this binary share `BORG_DATA_DIR` env + a global writer DB —
/// run them sequentially so each gets a clean slate.
static SERIAL: Mutex<()> = Mutex::new(());

/// Per-test unique env-var name suffix so api-key resolution can't bleed
/// across cases that race on key resolution before SERIAL is acquired.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// One scheduled-task dispatcher run-to-completion against a wiremock LLM.
/// Returns the request bodies wiremock observed plus the final
/// `task_runs.status` for the seeded task. Cleans up on drop.
struct DispatchHarness {
    server: MockServer,
    _data_dir: tempfile::TempDir,
    config: Config,
}

impl DispatchHarness {
    async fn setup() -> Self {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let env_key = format!("BORG_TEST_PRE_RUN_KEY_{n}");
        // SAFETY: SERIAL Mutex held by the caller; only this test mutates the
        // env. set_var is unsafe under Rust 2024 / safe under 2021 — match
        // the convention in `crates/cortex/tests/memory_token_cap.rs`.
        unsafe {
            std::env::set_var(&env_key, "test-key");
        }

        let data_dir = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("BORG_DATA_DIR", data_dir.path());
        }

        let server = MockServer::start().await;

        let mut config = Config::default();
        config.llm.provider = Some("openai".into());
        config.llm.api_key_env = env_key;
        config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
        config.llm.max_retries = 0;
        config.llm.request_timeout_ms = 10_000;
        // Task dispatcher runs heavyweight side things (memory, identity).
        // Keep them quiet for the test.
        config.memory.active_injection.enabled = false;
        config.memory.embeddings.enabled = false;

        Self {
            server,
            _data_dir: data_dir,
            config,
        }
    }

    /// Mount a wiremock that returns a single non-streaming OpenAI-shaped
    /// chat-completions response with the given assistant text.
    async fn mount_text_response(&self, text: &str) {
        let body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }]
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Insert a `scheduled_tasks` row, due now, with the given preamble.
    async fn seed_due_prompt_task(&self, id: &str, pre_run_command: Option<&str>) {
        let db = Database::open().await.expect("open db");
        let now = chrono::Utc::now().timestamp();
        db.create_task(&NewTask {
            id,
            name: id,
            prompt: "report status",
            schedule_type: "once",
            schedule_expr: "",
            timezone: "UTC",
            next_run: Some(now - 1),
            max_retries: Some(0),
            timeout_ms: Some(15_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command,
            pre_run_cwd: None,
            // Tight timeout for the preamble — `echo` and `exit` are instant.
            // Keeps test runtime bounded if the preamble hangs.
            pre_run_timeout_ms: Some(5_000),
            min_interval_secs: None,
        })
        .await
        .expect("create due prompt task");
    }

    /// Insert an *interval* `scheduled_tasks` row, due now, with the given
    /// preamble and monitor mode armed. Interval (not `once`) so the task
    /// stays active after a run and the test can re-arm `next_run` to drive
    /// a second and third dispatch against the same row.
    async fn seed_monitor_task(&self, id: &str, pre_run_command: &str) {
        let db = Database::open().await.expect("open db");
        let now = chrono::Utc::now().timestamp();
        db.create_task(&NewTask {
            id,
            name: id,
            prompt: "report status",
            schedule_type: "interval",
            schedule_expr: "5m",
            timezone: "UTC",
            next_run: Some(now - 1),
            max_retries: Some(0),
            timeout_ms: Some(15_000),
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: Some(pre_run_command),
            pre_run_cwd: None,
            pre_run_timeout_ms: Some(5_000),
            min_interval_secs: None,
        })
        .await
        .expect("create monitor task");
        assert!(
            db.set_task_monitor_mode(id, true)
                .await
                .expect("arm monitor"),
            "monitor flag must land on the seeded task"
        );
    }

    /// Spin up `scheduler::run` and poll until the task either succeeds in
    /// `task_runs` or the timeout elapses. Returns once the task row exists,
    /// regardless of status (the test asserts on captured request bodies).
    async fn run_until_task_recorded(self, task_id: &str) -> (MockServer, bool) {
        let recorded = self.run_until_n_runs(task_id, 1).await;
        (self.server, recorded)
    }

    /// Spin up `scheduler::run` and poll until `task_id` has at least `n`
    /// rows in `task_runs` (or 15s elapse). Borrows the harness so a test
    /// can dispatch the same row repeatedly, mutating state in between.
    async fn run_until_n_runs(&self, task_id: &str, n: usize) -> bool {
        let shutdown = CancellationToken::new();
        let semaphore = Arc::new(Semaphore::new(2));
        let hooks = scheduler::DispatcherHooks {
            spawn_task: Arc::new(|task, run_id, retry, sem, cfg| {
                Box::pin(async move {
                    task_runner::spawn_task_execution(task, run_id, retry, sem, cfg, None, None)
                        .await
                })
            }),
            deliver_celebration: Arc::new(|cfg, c| {
                Box::pin(async move { task_runner::deliver_celebration_to_channels(cfg, c).await })
            }),
        };
        let cfg = self.config.clone();
        let shutdown_for_run = shutdown.clone();
        let dispatcher = tokio::spawn(async move {
            // Liveness counter is the supervisor's stall signal in
            // production; this test drives `run` directly, so a local
            // counter is all it needs.
            let liveness = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
        });

        let mut recorded = false;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let db = Database::open().await.expect("db");
            if let Ok(rows) = db.task_run_history(task_id, 10).await {
                // A run row is inserted at *claim* time with status
                // 'running'; wait for the newest to reach a terminal state so
                // callers can assert on captured requests and ledger rows
                // without racing the in-flight execution.
                if rows.len() >= n && rows.first().is_some_and(|r| r.status != "running") {
                    recorded = true;
                    break;
                }
            }
        }

        // The terminal task_runs status lands just before trailing writes in
        // the same finalize block (e.g. the monitor baseline hash). Give the
        // spawned execution a beat to finish them before callers assert.
        tokio::time::sleep(Duration::from_millis(300)).await;

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;

        recorded
    }
}

/// Extract the chat-completions request bodies from a wiremock server. The
/// dispatcher may also issue probe / non-LLM requests in the future; filter
/// to the chat-completions path so assertions remain stable.
async fn captured_chat_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    let received = server.received_requests().await.unwrap_or_default();
    received
        .into_iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null))
        .collect()
}

/// Extract the user-message text from an OpenAI-shaped chat request body.
fn user_message_text(body: &serde_json::Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    let user = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    user.get("content")?.as_str().map(str::to_string)
}

// ─────────────────────────────────────────────────────────────────────────────
// Test cases
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn success_preamble_injects_preprocessed_context_block() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = DispatchHarness::setup().await;
    h.mount_text_response("ok").await;
    h.seed_due_prompt_task("preamble-success", Some("echo hello-from-preamble"))
        .await;

    let (server, recorded) = h.run_until_task_recorded("preamble-success").await;
    assert!(
        recorded,
        "dispatcher must record a task_runs row for the seeded prompt task"
    );

    let bodies = captured_chat_bodies(&server).await;
    assert!(
        !bodies.is_empty(),
        "wiremock must receive at least one chat-completions request"
    );
    let user = user_message_text(&bodies[0]).expect("first chat request must carry a user message");

    assert!(
        user.contains("<preprocessed_context source=\"pre_run\" exit_code=\"0\">"),
        "user prompt must open with the success preprocessed_context block; got:\n{user}"
    );
    assert!(
        user.contains("hello-from-preamble"),
        "preamble stdout must be embedded inside the block; got:\n{user}"
    );
    assert!(
        user.contains("</preprocessed_context>"),
        "block must be closed; got:\n{user}"
    );
    // The original task prompt must follow the block — proving order, not
    // merely co-existence.
    let block_end = user
        .find("</preprocessed_context>")
        .expect("close tag asserted above");
    let after_block = &user[block_end..];
    assert!(
        after_block.contains("report status"),
        "the original task prompt must follow the preprocessed_context block; \
         got after-block tail:\n{after_block}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_preamble_still_invokes_llm_with_failed_status() {
    // Invariant: a failing preamble MUST NOT abort the task. The LLM still
    // runs so the agent can surface the failure to the user. A silent skip
    // would leave the user wondering why their scheduled prompt produced
    // nothing.
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = DispatchHarness::setup().await;
    h.mount_text_response("noted").await;
    // `sh -c "exit 7"` exits with code 7; render_pre_run_block must surface
    // that as `status="failed" exit_code="7"`.
    h.seed_due_prompt_task("preamble-failure", Some("exit 7"))
        .await;

    let (server, recorded) = h.run_until_task_recorded("preamble-failure").await;
    assert!(
        recorded,
        "dispatcher must record a task_runs row even when the preamble exits non-zero"
    );

    let bodies = captured_chat_bodies(&server).await;
    assert!(
        !bodies.is_empty(),
        "preamble failure MUST NOT short-circuit the LLM call; \
         wiremock should have received a chat-completions request"
    );
    let user = user_message_text(&bodies[0]).expect("first chat request must carry a user message");

    assert!(
        user.contains("status=\"failed\""),
        "failed preamble must surface status=\"failed\" in the injected block; got:\n{user}"
    );
    assert!(
        user.contains("exit_code=\"7\""),
        "failed preamble must report the actual exit code (7); got:\n{user}"
    );
    // And the original prompt must still ride along behind the failure block.
    assert!(
        user.contains("report status"),
        "the original task prompt must follow the preprocessed_context block \
         even on preamble failure; got:\n{user}"
    );
}

/// Re-arm an interval task so the dispatcher claims it again immediately.
async fn make_due_again(task_id: &str) {
    let db = Database::open().await.expect("db");
    let now = chrono::Utc::now().timestamp();
    db.update_task_next_run(task_id, Some(now - 1))
        .await
        .expect("re-arm next_run");
}

async fn stored_pre_run_hash(task_id: &str) -> Option<String> {
    let db = Database::open().await.expect("db");
    db.get_task_by_id(task_id)
        .await
        .expect("get task")
        .expect("task exists")
        .last_pre_run_hash
}

/// Monitor mode, unchanged output: the first run of a monitor task must
/// execute (no baseline yet) and persist one; a re-run with byte-identical
/// pre-run output must NOT invoke the LLM, must be recorded as a no-change
/// run, and must leave the baseline hash untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_unchanged_pre_run_skips_llm_and_keeps_baseline() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = DispatchHarness::setup().await;
    h.mount_text_response("all quiet").await;
    h.seed_monitor_task("monitor-stable", "echo stable-output")
        .await;

    // First run: no baseline → must reach the LLM and store one.
    assert!(h.run_until_n_runs("monitor-stable", 1).await);
    assert_eq!(
        captured_chat_bodies(&h.server).await.len(),
        1,
        "first monitored run has no baseline and must invoke the LLM"
    );
    let baseline = stored_pre_run_hash("monitor-stable").await;
    assert!(
        baseline.is_some(),
        "first monitored run must persist the pre_run output hash as baseline"
    );

    // Second run: identical output → skip the LLM, record no-change.
    make_due_again("monitor-stable").await;
    assert!(h.run_until_n_runs("monitor-stable", 2).await);
    assert_eq!(
        captured_chat_bodies(&h.server).await.len(),
        1,
        "unchanged pre_run output must not invoke the LLM again"
    );
    let db = Database::open().await.expect("db");
    let runs = db.task_run_history("monitor-stable", 10).await.unwrap();
    assert_eq!(runs.len(), 2, "exactly two runs must be in the ledger");
    // `started_at` has second granularity, so the two runs may tie and come
    // back in either order — find the suppressed one by its marker.
    let no_change = runs
        .iter()
        .find(|r| r.result.as_deref().unwrap_or("").contains("no_change"))
        .unwrap_or_else(|| {
            panic!(
                "the run ledger must mark the suppressed run as no_change; got {:?}",
                runs.iter().map(|r| r.result.clone()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        no_change.status, "success",
        "a no-change run completed — it did exactly what monitoring asked"
    );
    assert_eq!(
        stored_pre_run_hash("monitor-stable").await,
        baseline,
        "a skipped run must not move the baseline"
    );
}

/// Monitor mode, changed output and pre-run failure: changed output runs the
/// LLM (with a change note) and rebaselines; a failing pre-run runs the LLM
/// normally and leaves the stored baseline untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_changed_output_reruns_and_failure_keeps_hash() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = DispatchHarness::setup().await;
    h.mount_text_response("noted").await;

    let state_dir = tempfile::tempdir().expect("state dir");
    let state_file = state_dir.path().join("state.txt");
    std::fs::write(&state_file, "v1\n").expect("write v1");
    // `cat` of a state file the test mutates between dispatches. Deleting the
    // file later makes the same command fail (non-zero) without editing the task.
    let cmd = format!("cat {}", state_file.display());
    h.seed_monitor_task("monitor-change", &cmd).await;

    // Run 1: baseline v1.
    assert!(h.run_until_n_runs("monitor-change", 1).await);
    assert_eq!(captured_chat_bodies(&h.server).await.len(), 1);
    let hash_v1 = stored_pre_run_hash("monitor-change")
        .await
        .expect("baseline v1");

    // Run 2: output changed → LLM runs again, baseline moves.
    std::fs::write(&state_file, "v2\n").expect("write v2");
    make_due_again("monitor-change").await;
    assert!(h.run_until_n_runs("monitor-change", 2).await);
    let bodies = captured_chat_bodies(&h.server).await;
    assert_eq!(
        bodies.len(),
        2,
        "changed pre_run output must invoke the LLM"
    );
    let user = user_message_text(&bodies[1]).expect("second run user message");
    assert!(
        user.contains("v2"),
        "the new output must be injected into the prompt; got:\n{user}"
    );
    assert!(
        user.contains("<monitor_note>"),
        "a changed monitored run must carry the change note; got:\n{user}"
    );
    let hash_v2 = stored_pre_run_hash("monitor-change")
        .await
        .expect("baseline v2");
    assert_ne!(hash_v2, hash_v1, "changed output must store a new baseline");

    // Run 3: pre-run fails (file gone) → LLM still runs (failure surfaced),
    // baseline untouched. A failure is not a change.
    std::fs::remove_file(&state_file).expect("remove state file");
    make_due_again("monitor-change").await;
    assert!(h.run_until_n_runs("monitor-change", 3).await);
    let bodies = captured_chat_bodies(&h.server).await;
    assert_eq!(
        bodies.len(),
        3,
        "a failing pre_run must run the task normally, never silently skip"
    );
    let user = user_message_text(&bodies[2]).expect("third run user message");
    assert!(
        user.contains("status=\"failed\""),
        "the pre_run failure must be surfaced to the model; got:\n{user}"
    );
    assert_eq!(
        stored_pre_run_hash("monitor-change").await.as_deref(),
        Some(hash_v2.as_str()),
        "a failed pre_run must leave the stored baseline untouched"
    );
}
