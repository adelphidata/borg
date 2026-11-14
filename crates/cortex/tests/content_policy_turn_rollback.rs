//! Integration test: a turn the provider refuses on content grounds is taken
//! out of the conversation.
//!
//! Safety filters judge the whole thread, so a refused request left in
//! history gets every later message refused too. Drives the full
//! `Agent::send_message` path against a wiremock LLM: the first turn is
//! refused, the second must succeed without re-sending the refused text.

// Drives the whole agent loop, whose future type nests deep enough that layout
// computation exceeds the default query depth.
#![recursion_limit = "256"]
#![allow(
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used
)]

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::agent::{Agent, AgentEvent};
use cortex::config::Config;
use cortex::telemetry::BorgMetrics;

const REFUSED: &str = "the request the provider refuses";

fn text_sse(text: &str) -> String {
    [
        format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
        "[DONE]".to_string(),
    ]
    .iter()
    .map(|l| format!("data: {l}\n\n"))
    .collect()
}

async fn send(agent: &mut Agent, prompt: &str) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    // A refused turn may surface as Ok (error event) or Err; either way the
    // history is what this test inspects.
    let _ = agent
        .send_message_with_cancel(prompt, tx, CancellationToken::new())
        .await;
    drain.abort();
}

#[tokio::test]
async fn a_refused_turn_is_removed_so_the_next_one_goes_through() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    std::env::set_var("BORG_HOME", data_dir.path());
    std::env::set_var("BORG_TEST_CONTENT_POLICY_KEY", "test-key-value");
    let server = MockServer::start().await;

    // Any request that still carries the refused text is refused again,
    // which is what a real filter does to the whole thread.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(REFUSED))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Input data may contain inappropriate content.","code":"data_inspection_failed"}}"#,
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse("happy to help"), "text/event-stream"),
        )
        .with_priority(2)
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = "BORG_TEST_CONTENT_POLICY_KEY".into();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.max_retries = 0;
    config.conversation.max_iterations = 4;
    config.conversation.concurrent_tools.enabled = false;
    config.memory.active_injection.enabled = false;
    config.memory.embeddings.enabled = false;

    let mut agent = Agent::new(config, BorgMetrics::default())
        .await
        .expect("Agent::new");

    send(&mut agent, REFUSED).await;
    assert!(
        !agent
            .history()
            .iter()
            .any(|m| m.text_content().is_some_and(|t| t.contains(REFUSED))),
        "the refused request must be taken out of history: {:?}",
        agent.history()
    );

    send(&mut agent, "a different question").await;
    let last = agent.history().last().expect("an answer");
    assert_eq!(last.text_content(), Some("happy to help"));
}
