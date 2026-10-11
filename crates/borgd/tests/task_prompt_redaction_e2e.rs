//! Integration test: a scheduled prompt task's result and failure text are
//! redacted before they are persisted.
//!
//! Spec invariant (AGENTS.md → "Prompt Injection", layer 5 "secret
//! redaction"): secrets must not survive into anything Borg stores or sends.
//!
//! A prompt task reads untrusted content and can repeat a secret it saw. The
//! stored `task_runs.result` is what continuity replays and what the delivery
//! lane sends, and the stored `error` / `last_error` is what the failure
//! notice sends. This drives the real `scheduler::run` loop and the real
//! `task_runner::spawn_task_execution` against a wiremock LLM, then reads the
//! stored rows back.
//!
//! ── Mutation verification ──
//!
//! • In `execute_prompt_task`, pass `response.text_content()` to
//!   `complete_task_run` without `redact_for_run`:
//!   `prompt_result_with_a_token_is_stored_redacted` fails.
//! • In `handle_task_failure`, drop the `redact_for_run` rebinding of
//!   `error`: `prompt_failure_with_a_token_is_stored_redacted` fails.
//! • In `execute_prompt_task`, store the LLM-init error `format!("{e}")`
//!   without `redact_for_run`:
//!   `llm_setup_error_with_a_token_is_stored_redacted` fails.
//! • In `execute_heartbeat_task`, store the LLM-init error `format!("{e}")`
//!   without `redact_for_run`: the heartbeat row of
//!   `llm_setup_error_with_a_token_is_stored_redacted` fails.

#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used,
    clippy::uninlined_format_args,
    clippy::await_holding_lock
)]

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use borgd::{scheduler, task_runner};
use cortex::config::Config;
use cortex::db::{Database, NewTask, TaskRunRow};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Tests in this binary share the process-global `BORG_HOME` env var.
static SERIAL: Mutex<()> = Mutex::new(());

/// A GitHub-token-shaped string: `ghp_` + 36 chars.
const FAKE_TOKEN: &str = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

/// The part that must not survive. A redactor that masked only the `ghp_`
/// prefix would still fail.
const TOKEN_BODY: &str = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

struct Harness {
    server: MockServer,
    _data_dir: tempfile::TempDir,
    config: Config,
}

impl Harness {
    async fn setup(env_key: &str) -> Self {
        let data_dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: SERIAL is held by the caller; no other test in this binary
        // touches the environment concurrently.
        unsafe {
            std::env::set_var(env_key, "test-key");
            std::env::set_var("BORG_HOME", data_dir.path());
        }
        let server = MockServer::start().await;

        let mut config = Config::default();
        config.llm.provider = Some("openai".into());
        config.llm.api_key_env = env_key.to_string();
        config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
        config.llm.max_retries = 0;
        config.llm.request_timeout_ms = 10_000;
        config.memory.active_injection.enabled = false;
        config.memory.embeddings.enabled = false;

        Self {
            server,
            _data_dir: data_dir,
            config,
        }
    }

    async fn mount(&self, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    async fn seed_due_prompt_task(&self, id: &str) {
        self.seed_due_task(id, "prompt").await;
    }

    async fn seed_due_task(&self, id: &str, task_type: &str) {
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
            task_type,
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        })
        .await
        .expect("create due prompt task");
    }

    /// Run the dispatcher until the task has a finished run row.
    async fn run_to_finished_row(&self, task_id: &str) -> TaskRunRow {
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
            shutdown_process: Arc::new(|| {}),
        };
        let cfg = self.config.clone();
        let shutdown_for_run = shutdown.clone();
        let dispatcher = tokio::spawn(async move {
            let liveness = Arc::new(AtomicU64::new(0));
            scheduler::run(cfg, hooks, semaphore, shutdown_for_run, liveness).await
        });

        let mut finished = None;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let db = Database::open().await.expect("db");
            let row = db.last_task_run(task_id).await.expect("query run");
            if let Some(row) = row.filter(|r| r.status != "running") {
                finished = Some(row);
                break;
            }
        }
        // The failure lane writes `last_error` just after the run row.
        tokio::time::sleep(Duration::from_millis(300)).await;

        shutdown.cancel();
        if tokio::time::timeout(Duration::from_secs(2), dispatcher)
            .await
            .is_err()
        {
            eprintln!("dispatcher did not stop within 2s of cancel");
        }
        finished.expect("the dispatcher must finish a run for the seeded task")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_result_with_a_token_is_stored_redacted() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = Harness::setup("BORG_TEST_PROMPT_REDACT_KEY_OK").await;
    h.mount(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": format!("Deploy is green. The page showed token {FAKE_TOKEN} in a log.")
            },
            "finish_reason": "stop"
        }]
    })))
    .await;
    h.seed_due_prompt_task("prompt-leak").await;

    let run = h.run_to_finished_row("prompt-leak").await;

    assert_eq!(run.error, None, "the run must succeed: {run:?}");
    let stored = run.result.expect("a successful run stores its result");
    assert!(
        !stored.contains(TOKEN_BODY),
        "task_runs.result must not hold the token: {stored}"
    );
    assert!(
        stored.starts_with("Deploy is green. The page showed token [REDACTED")
            && stored.ends_with(" in a log."),
        "only the secret may be cut: {stored}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_failure_with_a_token_is_stored_redacted() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = Harness::setup("BORG_TEST_PROMPT_REDACT_KEY_FAIL").await;
    // A provider that repeats the credential in its error body.
    h.mount(ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {
            "message": format!("bad request for credential {FAKE_TOKEN}"),
            "type": "invalid_request_error"
        }
    })))
    .await;
    h.seed_due_prompt_task("prompt-fail-leak").await;

    let run = h.run_to_finished_row("prompt-fail-leak").await;

    let stored = run.error.expect("a failed run stores its error");
    // Guard against a vacuous pass: the provider text must reach the stored
    // error, or this test does not exercise the redaction at all.
    assert!(
        stored.contains("bad request for credential"),
        "the provider message must be part of the stored error: {stored}"
    );
    assert!(
        !stored.contains(TOKEN_BODY),
        "task_runs.error must not hold the token: {stored}"
    );
    assert!(stored.contains("[REDACTED"), "{stored}");

    let db = Database::open().await.expect("db");
    let task = db
        .get_task_by_id("prompt-fail-leak")
        .await
        .expect("query task")
        .expect("task row");
    let last_error = task.last_error.expect("a failed task keeps last_error");
    assert!(
        !last_error.contains(TOKEN_BODY),
        "scheduled_tasks.last_error must not hold the token: {last_error}"
    );
}

/// LLM client setup fails before any request: a key pasted into the provider
/// field comes back in the "Unknown provider" error, which must not be
/// stored as-is — on the prompt lane and on the heartbeat lane alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_setup_error_with_a_token_is_stored_redacted() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    for task_type in ["prompt", "heartbeat"] {
        let mut h = Harness::setup("BORG_TEST_PROMPT_REDACT_KEY_SETUP").await;
        h.config.llm.provider = Some(FAKE_TOKEN.into());
        // The default quiet hours would skip the heartbeat before setup.
        h.config.heartbeat.quiet_hours_start = None;
        h.config.heartbeat.quiet_hours_end = None;
        let id = format!("{task_type}-setup-leak");
        h.seed_due_task(&id, task_type).await;

        let run = h.run_to_finished_row(&id).await;

        let stored = run
            .error
            .unwrap_or_else(|| panic!("{task_type}: a failed setup stores its error"));
        assert!(
            stored.starts_with("Unknown provider: "),
            "{task_type}: the setup error must be what was stored: {stored}"
        );
        assert!(
            !stored.contains(TOKEN_BODY),
            "{task_type}: task_runs.error must not hold the token: {stored}"
        );
        assert!(stored.contains("[REDACTED"), "{task_type}: {stored}");
        assert!(
            h.server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "{task_type}: setup must fail before any request"
        );
    }
}

/// A prompt task is one tool-less completion. The model must be told that,
/// or "read this file" comes back with invented contents delivered as fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_task_tells_the_model_it_has_no_tools() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let h = Harness::setup("BORG_TEST_PROMPT_NO_TOOLS_KEY").await;
    h.mount(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "status unknown"},
            "finish_reason": "stop"
        }]
    })))
    .await;
    h.seed_due_prompt_task("prompt-no-tools").await;
    h.run_to_finished_row("prompt-no-tools").await;

    let requests = h.server.received_requests().await.expect("recorded");
    let body: serde_json::Value = requests
        .iter()
        .find(|r| r.url.path() == "/v1/chat/completions")
        .expect("the task reached the LLM")
        .body_json()
        .expect("json body");
    assert!(body.get("tools").is_none(), "no tools are offered: {body}");
    let system = body["messages"][0]["content"].as_str().unwrap_or_default();
    assert!(
        system.contains(task_runner::SCHEDULED_TASK_NO_TOOLS),
        "system prompt must state the no-tools contract: {system}"
    );
}
