//! Integration test: Plan-mode collaboration mode blocks mutating tool calls.
//!
//! Spec invariant (AGENTS.md, "Collaboration Modes"):
//!   When `config.conversation.collaboration_mode = Plan`, an LLM-emitted tool
//!   call to a mutating tool is rejected by the agent loop with a result
//!   containing "Plan mode:", and the tool dispatcher is never invoked.
//!
//! This drives the full `Agent::send_message` → `run_agent_loop` path against
//! a wiremock-backed LLM endpoint. The LLM emits a `write_memory` tool call
//! (mutating); we verify the rejection appears in `agent.history()` for Plan
//! mode and does NOT appear for Execute mode (control case proving the gate
//! is mode-conditional, not always-on).
//!
//! The only proof of "tool was not executed" we assert on is the rejection
//! message itself (produced solely by `skip_tool_call` with the literal
//! "Plan mode:" prefix at agent/mod.rs gate sites). The complementary
//! Execute-mode case asserts that `<tool_output name="write_memory">` appears
//! in history — which only happens after the dispatcher runs the tool —
//! proving the gate is what's stopping execution in the Plan-mode case.

// Drives the whole agent loop, whose future type nests deep enough that layout
// computation exceeds the default query depth.
#![recursion_limit = "256"]
#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use std::sync::atomic::{AtomicU32, Ordering};

use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::agent::{Agent, AgentEvent};
use cortex::config::media::CollaborationMode;
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// SSE body framing: each line becomes `data: {line}\n\n`.
fn sse_body(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| format!("data: {l}\n\n"))
        .collect::<String>()
}

/// SSE response that emits a single tool call and a stop finish.
fn tool_call_sse(call_id: &str, tool_name: &str, args_json: &str) -> String {
    // Escape `"` in args_json for embedding inside the JSON delta.
    let args_escaped = args_json.replace('\\', r"\\").replace('"', r#"\""#);
    sse_body(&[
        &format!(
            r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","function":{{"name":"{tool_name}","arguments":""}}}}]}},"finish_reason":null}}]}}"#,
        ),
        &format!(
            r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":"{args_escaped}"}}}}]}},"finish_reason":null}}]}}"#,
        ),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ])
}

/// SSE response that emits final assistant text and a stop finish.
fn text_sse(text: &str) -> String {
    sse_body(&[
        &format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#,),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ])
}

struct Harness {
    _data_dir: TempDir,
    _server: MockServer,
    config: Config,
}

/// Stand up a wiremock LLM that returns the given tool call on the first
/// `POST /v1/chat/completions`, then a text response on every subsequent call.
/// Returns a `Config` wired to that endpoint plus a TempDir for `BORG_HOME`.
async fn harness(call_id: &str, tool_name: &str, args_json: &str) -> Harness {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_PLAN_MODE_KEY_{n}");
    std::env::set_var(&env_key, "test-key-value");

    // Route every borg-data write into a per-test temp dir. Without this, the
    // agent's call to `Database::open()` writes to `~/.borg/borg.db`, which
    // would cross-contaminate the user's real DB and parallel test runs.
    let data_dir = tempfile::tempdir().expect("temp data dir");
    std::env::set_var("BORG_HOME", data_dir.path());

    let server = MockServer::start().await;

    // First call: the tool call. `up_to_n_times(1)` makes wiremock fall through
    // to the next mock on subsequent requests.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            tool_call_sse(call_id, tool_name, args_json),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Subsequent calls (the agent loops back after processing the tool result):
    // emit a final text response with finish_reason=stop. The agent then exits.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse("done"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = env_key;
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.max_retries = 0;
    // Keep the loop bounded — if either gate misbehaves we get a fast failure
    // instead of a hung test.
    config.conversation.max_iterations = 4;
    // Disable conditional tool loading so write_memory's schema is always
    // visible to the LLM. (We're feeding canned SSE so the LLM doesn't need
    // the schema, but the agent's request-prep path still references it.)
    config.conversation.concurrent_tools.enabled = false;
    // Keep proactive memory injection off — the test should not depend on
    // embedding-cache / vector-search behavior.
    config.memory.active_injection.enabled = false;
    config.memory.embeddings.enabled = false;

    Harness {
        _data_dir: data_dir,
        _server: server,
        config,
    }
}

/// Drive one agent turn end-to-end and return the resulting history.
async fn run_one_turn(mut config: Config, prompt: &str) -> Vec<cortex::types::Message> {
    let mut agent = Agent::new(config.clone(), BorgMetrics::default())
        .await
        .expect("Agent::new");
    // `Agent::new` clones the config internally; the local `config` binding is
    // unused after this. (We keep it as `mut` for symmetry with future cases
    // that may toggle further fields after construction.)
    let _ = &mut config;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);

    // Drain events on a background task; the agent loop blocks on tx.send
    // when the channel is full, so we must consume.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    agent
        .send_message_with_cancel(prompt, tx, CancellationToken::new())
        .await
        .expect("send_message");

    drain.abort();
    agent.history().to_vec()
}

/// True if the message is a tool result whose body starts with the Plan-mode
/// rejection prefix.
fn is_plan_mode_rejection(msg: &cortex::types::Message) -> bool {
    if !matches!(msg.role, cortex::types::Role::Tool) {
        return false;
    }
    let Some(text) = msg.text_content() else {
        return false;
    };
    text.starts_with("Plan mode:")
}

/// True if the message is a tool result whose body contains a `<tool_output
/// name="<tool>" ...>` block — i.e. the dispatcher actually ran the tool.
fn contains_tool_output_block(msg: &cortex::types::Message, tool_name: &str) -> bool {
    if !matches!(msg.role, cortex::types::Role::Tool) {
        return false;
    }
    let Some(text) = msg.text_content() else {
        return false;
    };
    text.contains(&format!("<tool_output name=\"{tool_name}\""))
}

// ─────────────────────────────────────────────────────────────────────────────
// Test cases
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plan_mode_blocks_write_memory_tool_call() {
    let mut h = harness(
        "call_block_wm",
        "write_memory",
        r#"{"scope":"global","name":"NOTES","content":"hello"}"#,
    )
    .await;
    h.config.conversation.collaboration_mode = CollaborationMode::Plan;

    let history = run_one_turn(h.config.clone(), "save a note").await;

    // Exactly one tool result; it must be the Plan-mode rejection.
    let tool_results: Vec<&cortex::types::Message> = history
        .iter()
        .filter(|m| matches!(m.role, cortex::types::Role::Tool))
        .collect();
    assert!(
        !tool_results.is_empty(),
        "expected at least one tool-result message in history; got {} messages: {:?}",
        history.len(),
        history.iter().map(|m| &m.role).collect::<Vec<_>>(),
    );
    assert!(
        tool_results.iter().any(|m| is_plan_mode_rejection(m)),
        "Plan mode must produce a tool result starting with 'Plan mode:'; \
         got tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
    // And the dispatcher must not have run the tool — no <tool_output> block.
    assert!(
        !tool_results
            .iter()
            .any(|m| contains_tool_output_block(m, "write_memory")),
        "Plan mode must NOT execute write_memory: a <tool_output name=\"write_memory\"> \
         block in history would mean the dispatcher ran. Tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn execute_mode_does_not_block_write_memory_tool_call() {
    // Control case: same wiremock script, same tool call, but Execute mode.
    // The gate must NOT fire — no "Plan mode:" rejection in history. The tool
    // itself may succeed or fail (write_memory needs DB plumbing this test
    // doesn't fully exercise) but it must reach the dispatcher, which means
    // a <tool_output name="write_memory"> block lands in history regardless
    // of the inner outcome.
    let h = harness(
        "call_exec_wm",
        "write_memory",
        r#"{"scope":"global","name":"NOTES","content":"hello"}"#,
    )
    .await;
    // Default is Execute, but be explicit — the test's spec is "Execute mode
    // does not block".
    let mut config = h.config.clone();
    config.conversation.collaboration_mode = CollaborationMode::Execute;

    let history = run_one_turn(config, "save a note").await;

    let tool_results: Vec<&cortex::types::Message> = history
        .iter()
        .filter(|m| matches!(m.role, cortex::types::Role::Tool))
        .collect();
    assert!(
        !tool_results.iter().any(|m| is_plan_mode_rejection(m)),
        "Execute mode must NOT emit a Plan-mode rejection. Tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
    assert!(
        tool_results
            .iter()
            .any(|m| contains_tool_output_block(m, "write_memory")),
        "Execute mode must reach the dispatcher: history should contain a \
         <tool_output name=\"write_memory\"> block. Tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn plan_mode_does_not_block_read_only_tool_call() {
    // Control case: in Plan mode, a non-mutating tool (read_file) must reach
    // the dispatcher. This proves the gate is allowlist-driven (only mutating
    // tools blocked) rather than blocking everything in Plan mode.
    let mut h = harness("call_pm_rf", "read_file", r#"{"path":"Cargo.toml"}"#).await;
    h.config.conversation.collaboration_mode = CollaborationMode::Plan;

    let history = run_one_turn(h.config.clone(), "show the cargo manifest").await;

    let tool_results: Vec<&cortex::types::Message> = history
        .iter()
        .filter(|m| matches!(m.role, cortex::types::Role::Tool))
        .collect();
    assert!(
        !tool_results.iter().any(|m| is_plan_mode_rejection(m)),
        "Plan mode must NOT block read_file (non-mutating). Tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
    assert!(
        tool_results
            .iter()
            .any(|m| contains_tool_output_block(m, "read_file")),
        "Plan mode must reach the read_file dispatcher: history should contain a \
         <tool_output name=\"read_file\"> block. Tool results: {:?}",
        tool_results
            .iter()
            .map(|m| m.text_content().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
}
