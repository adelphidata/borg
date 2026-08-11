//! Integration test: output-token-cap recovery in the agent loop.
//!
//! Spec invariant: when a response finishes with `finish_reason = "length"`
//! (the provider's max_tokens cap), the agent compacts history once and
//! re-issues the iteration instead of persisting a `[stream interrupted:
//! response truncated at max_tokens…]` marker — mirroring the reactive
//! compaction path that already exists for *input* context overflow. A
//! second truncation in the same turn falls back to the marker so the loop
//! cannot ping-pong.
//!
//! Drives the full `Agent::send_message` → `run_agent_loop` path against a
//! wiremock LLM. History is pre-seeded between the reactive-compaction
//! target (0.6 × budget) and the proactive threshold (1.0 × budget) so the
//! pre-send proactive pass leaves it alone while the truncation retry has
//! real tokens to reclaim.

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
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn sse_body(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| format!("data: {l}\n\n"))
        .collect::<String>()
}

/// SSE response cut off by the provider's output-token cap.
fn truncated_sse(text: &str) -> String {
    sse_body(&[
        &format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#,),
        r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        "[DONE]",
    ])
}

/// SSE response that completes normally.
fn text_sse(text: &str) -> String {
    sse_body(&[
        &format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#,),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ])
}

struct Harness {
    _data_dir: TempDir,
    server: MockServer,
    config: Config,
}

/// Config + wiremock endpoint with a small history budget so compaction has
/// something to do at test scale. Mocks are mounted by each test.
async fn harness() -> Harness {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_OUTPUT_CAP_KEY_{n}");
    std::env::set_var(&env_key, "test-key-value");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    std::env::set_var("BORG_HOME", data_dir.path());

    let server = MockServer::start().await;

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = env_key;
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.max_retries = 0;
    config.conversation.max_iterations = 4;
    // Small budget so seeded history lands between the reactive target
    // (0.6 × 2000) and the proactive threshold (2000) without megabytes of
    // filler.
    config.conversation.max_history_tokens = 2000;
    config.conversation.concurrent_tools.enabled = false;
    config.memory.active_injection.enabled = false;
    config.memory.embeddings.enabled = false;

    Harness {
        _data_dir: data_dir,
        server,
        config,
    }
}

/// Seed the agent's history with compactable bulk: assistant prose totaling
/// ~0.75 × budget spread across enough messages that head protection
/// (`protect_first_n = 3`) and tail protection leave a real middle region
/// for the truncation retry's compaction pass to reclaim.
async fn seed_bulk_history(agent: &mut Agent) {
    // 12 messages × ~145 BPE tokens each ≈ 1700 total: above the reactive
    // target (0.6 × 2000 = 1200) and below the proactive threshold (2000).
    let filler = "The deploy pipeline logs showed a series of warnings about \
                  retry budgets and cache misses across several services. "
        .repeat(6);
    for _ in 0..12 {
        agent.append_assistant_message(&filler).await;
    }
}

async fn run_one_turn(config: Config, prompt: &str) -> (Vec<cortex::types::Message>, Agent) {
    let mut agent = Agent::new(config, BorgMetrics::default())
        .await
        .expect("Agent::new");
    seed_bulk_history(&mut agent).await;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    agent
        .send_message_with_cancel(prompt, tx, CancellationToken::new())
        .await
        .expect("send_message");
    drain.abort();
    let history = agent.history().to_vec();
    (history, agent)
}

fn contains_truncation_marker(history: &[cortex::types::Message]) -> bool {
    history.iter().any(|m| {
        m.text_content()
            .is_some_and(|t| t.contains("response truncated at max_tokens"))
    })
}

#[tokio::test]
async fn output_cap_truncation_compacts_and_retries_once() {
    let h = harness().await;

    // First call: truncated at the output cap. Every subsequent call (the
    // compaction summarizer + the retried iteration) completes normally.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            truncated_sse("this answer was cut off mid-"),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(text_sse("complete answer"), "text/event-stream"),
        )
        .mount(&h.server)
        .await;

    let (history, _agent) = run_one_turn(h.config.clone(), "summarize the deploy status").await;

    assert!(
        !contains_truncation_marker(&history),
        "a recovered truncation must not persist the interrupted marker; history: {:#?}",
        history
            .iter()
            .map(|m| m
                .text_content()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(
        history.iter().any(|m| m
            .text_content()
            .is_some_and(|t| t.contains("complete answer"))),
        "the retried iteration's complete answer must be the turn's outcome"
    );
    // The truncated partial must not have been persisted as a final answer.
    assert!(
        !history.iter().any(|m| m
            .text_content()
            .is_some_and(|t| t.trim_end().ends_with("cut off mid-"))),
        "the truncated partial must be discarded, not persisted"
    );
}

#[tokio::test]
async fn second_truncation_falls_back_to_marker() {
    let h = harness().await;

    // Every model call truncates. The compaction summarizer also receives a
    // truncated response — its text is still non-empty, so compaction
    // succeeds and the retry proceeds, truncating again.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(truncated_sse("always cut off"), "text/event-stream"),
        )
        .mount(&h.server)
        .await;

    let (history, _agent) = run_one_turn(h.config.clone(), "summarize the deploy status").await;

    assert!(
        contains_truncation_marker(&history),
        "a second truncation in the same turn must surface the marker"
    );
}
