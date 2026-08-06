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

    /// Spin up `scheduler::run` and poll until the task either succeeds in
    /// `task_runs` or the timeout elapses. Returns once the task row exists,
    /// regardless of status (the test asserts on captured request bodies).
    async fn run_until_task_recorded(self, task_id: &str) -> (MockServer, bool) {
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
            if let Ok(rows) = db.task_run_history(task_id, 5).await {
                if !rows.is_empty() {
                    recorded = true;
                    break;
                }
            }
        }

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;

        (self.server, recorded)
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
