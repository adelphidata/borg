//! Integration tests for LLM SSE stream parsing via wiremock.
//!
//! Each test spins up a local HTTP server returning canned SSE responses,
//! points `LlmClient` at it via `Config.llm.base_url`, and verifies the
//! `StreamEvent` sequence produced by `stream_chat`.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use borg_core::config::Config;
use borg_core::llm::{LlmClient, StreamEvent};
use borg_core::types::{Message, MessageContent, Role};

/// Monotonic counter to generate unique env var names per test, avoiding races.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Build a `Config` that routes LLM requests to the given wiremock server.
fn test_config(server_uri: &str) -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_LLM_KEY_{n}");
    // Each test uses a unique env var name to avoid conflicts in parallel execution.
    std::env::set_var(&env_key, "test-key-value");

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = env_key;
    config.llm.base_url = Some(format!("{server_uri}/v1/chat/completions"));
    config.llm.max_retries = 0; // no retries for fast tests
    config
}

/// Simple user message for test payloads.
fn user_message(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: Some(MessageContent::Text(text.to_string())),
        tool_calls: None,
        tool_call_id: None,
        timestamp: None,
    }]
}

/// Build an SSE body from data lines (each line becomes `data: {line}\n\n`).
fn sse_body(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| format!("data: {l}\n\n"))
        .collect::<String>()
}

/// Collect all `StreamEvent`s from the receiver into a vec.
async fn collect_events(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<String> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::TextDelta(t) => events.push(format!("text:{t}")),
            StreamEvent::ThinkingDelta(t) => events.push(format!("thinking:{t}")),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                events.push(format!(
                    "tool:{index}:{}:{}:{arguments_delta}",
                    id.as_deref().unwrap_or(""),
                    name.as_deref().unwrap_or(""),
                ));
            }
            StreamEvent::Usage(u) => {
                events.push(format!(
                    "usage:{}:{}:{}",
                    u.prompt_tokens, u.completion_tokens, u.total_tokens
                ));
            }
            StreamEvent::Done => {
                events.push("done".into());
                break;
            }
            StreamEvent::Error(e) => {
                events.push(format!("error:{e}"));
                break;
            }
        }
    }
    events
}

// ── Test: text-only response ──

#[tokio::test]
async fn text_only_response() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert_eq!(events, vec!["text:Hello", "text: world", "done"]);
}

// ── Test: single tool call ──

#[tokio::test]
async fn single_tool_call() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"read_memory","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"filename\":"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"test.md\"}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("read my memory"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert_eq!(events.len(), 4); // 3 tool deltas + done
    assert!(events[0].starts_with("tool:0:call_abc:read_memory:"));
    assert!(events[1].starts_with("tool:0:::{\"filename\":"));
    assert!(events[2].starts_with("tool:0:::\"test.md\"}"));
    assert_eq!(events[3], "done");
}

// ── Test: parallel tool calls ──

#[tokio::test]
async fn parallel_tool_calls() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        // First tool call at index 0
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_memory","arguments":"{}"}}]},"finish_reason":null}]}"#,
        // Second tool call at index 1
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"run_shell","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("do two things"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert_eq!(events.len(), 3); // 2 tool deltas + done
    assert!(events[0].contains("call_1") && events[0].contains("read_memory"));
    assert!(events[1].contains("call_2") && events[1].contains("run_shell"));
    assert_eq!(events[2], "done");
}

// ── Test: usage data extracted ──

#[tokio::test]
async fn usage_data_extracted() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":25,"total_tokens":125}}"#,
        "[DONE]",
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("count"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    // Usage is emitted before finish_reason triggers Done
    assert!(events.contains(&"text:ok".to_string()));
    assert!(events.contains(&"usage:100:25:125".to_string()));
    assert!(events.last().map(|e| e == "done").unwrap_or(false));
}

// ── Test: rate limit 429 ──

#[tokio::test]
async fn rate_limit_429() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(r#"{"error":{"message":"rate limited"}}"#),
        )
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    let result = client.stream_chat(&user_message("hi"), None, tx).await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("429") || err_msg.contains("rate"),
        "Expected rate limit error, got: {err_msg}"
    );

    // Error event may also be sent on the channel (depends on when the error occurs)
    let events = collect_events(rx).await;
    if !events.is_empty() {
        assert!(events.iter().any(|e| e.starts_with("error:")));
    }
}

// ── Test: auth error 401 ──

#[tokio::test]
async fn auth_error_401() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"invalid api key"}}"#),
        )
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    let result = client.stream_chat(&user_message("hi"), None, tx).await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("401") || err_msg.contains("auth"),
        "Expected auth error, got: {err_msg}"
    );

    let events = collect_events(rx).await;
    if !events.is_empty() {
        assert!(events.iter().any(|e| e.starts_with("error:")));
    }
}

// ── Test: empty stream (just [DONE]) ──

#[tokio::test]
async fn empty_stream() {
    let server = MockServer::start().await;

    let body = sse_body(&["[DONE]"]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat should succeed on empty stream");

    let events = collect_events(rx).await;
    assert_eq!(events, vec!["done"]);
}

// ── Test: usage-only response (no choices, just usage + [DONE]) ──

#[tokio::test]
async fn usage_only_no_choices() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#,
        r#"{"usage":{"prompt_tokens":50,"completion_tokens":10,"total_tokens":60}}"#,
        "[DONE]",
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(events.contains(&"text:hi".to_string()));
    assert!(events.contains(&"usage:50:10:60".to_string()));
    assert!(events.last().map(|e| e == "done").unwrap_or(false));
}

// ── Test: server error 500 ──

#[tokio::test]
async fn server_error_500() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string(r#"{"error":{"message":"internal server error"}}"#),
        )
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    let result = client.stream_chat(&user_message("hi"), None, tx).await;

    // 500 is retryable but max_retries=0, so it should fail
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("500"),
        "Expected 500 error, got: {err_msg}"
    );

    let events = collect_events(rx).await;
    if !events.is_empty() {
        assert!(events.iter().any(|e| e.starts_with("error:")));
    }
}

// ── Test: provider closes connection without trailing newline ──
//
// Some providers (notably OpenAI-compatible gateways behind CORS/HTTP-2
// intermediaries) drop the final `\n\n` before the socket closes. The SSE flush
// path must emit the last delta instead of silently discarding it. Exercises
// `sse.rs`'s EOF-flush through the full LlmClient → process_sse_stream
// integration.
#[tokio::test]
async fn eof_without_trailing_newline_flushes_last_delta() {
    let server = MockServer::start().await;

    // Raw body — NOT using sse_body(), because that helper appends \n\n.
    // The final `[DONE]` has no trailing newline, simulating a truncated socket.
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" last\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]",
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let config = test_config(&server.uri());
    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert_eq!(
        events,
        vec!["text:first", "text: last", "done"],
        "EOF-flush path must surface the final delta and terminate cleanly"
    );
}
