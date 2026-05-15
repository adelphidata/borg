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
use std::sync::LazyLock;

use tokio::sync::mpsc;
use tokio::sync::Mutex;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::config::Config;
use cortex::llm::{LlmClient, StreamEvent};
use cortex::types::{Message, MessageContent, Role};

/// Monotonic counter to generate unique env var names per test, avoiding races.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);
static CLOUD_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Build a `Config` that routes LLM requests to the given wiremock server.
fn test_config(server_uri: &str) -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_LLM_KEY_{n}");
    // Each test uses a unique env var name to avoid conflicts in parallel execution.
    std::env::set_var(&env_key, format!("test-key-value-{n}"));

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
            StreamEvent::Truncated => {
                events.push("truncated".into());
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

// ── Test: xAI (Grok) routes through OpenAI-compat transport ──

/// Verify that `Provider::XAi` is wired through `OpenAiCompatTransport` end to
/// end: the request hits `/v1/chat/completions`, carries `Authorization: Bearer
/// xai-…`, advertises the configured Grok model, and the streaming SSE response
/// parses through the same path as OpenAI/Groq/DeepSeek. If this test fails,
/// either the provider was mis-classified as Anthropic-shaped or the auth
/// header path skipped the new Bearer arm.
#[tokio::test]
async fn xai_streams_via_openai_compat_transport() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"hi from Grok"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#,
        "[DONE]",
    ]);

    // Header matchers fail-closed: if the auth header is missing or x-api-key
    // is sent instead, the mock won't match and the streaming call errors out.
    // That covers the "did xAI route through the OpenAI-compat Bearer arm"
    // invariant without needing post-hoc HeaderName/HeaderValues plumbing.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer xai-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_XAI_KEY_{n}");
    std::env::set_var(&env_key, "xai-test-key");

    let mut config = Config::default();
    config.llm.provider = Some("xai".into());
    config.llm.api_key_env = env_key.clone();
    config.llm.base_url = Some(format!("{}/v1/chat/completions", server.uri()));
    config.llm.model = "grok-4-fast-non-reasoning".into();
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(
        events.iter().any(|e| e == "text:hi from Grok"),
        "expected the Grok delta to surface as a TextDelta event, got {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "usage:7:3:10"),
        "expected usage extraction, got {events:?}"
    );
    assert_eq!(
        events.last().map(|s| s.as_str()),
        Some("done"),
        "stream must terminate with Done, got {events:?}"
    );

    // The header matcher above already proved the request carried
    // `Authorization: Bearer xai-test-key`. Inspect the body to verify the
    // configured Grok slug + streaming flag actually made it onto the wire.
    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "expected exactly one POST");
    let req = &received[0];
    let body_json: serde_json::Value =
        serde_json::from_slice(&req.body).expect("request body is JSON");
    assert_eq!(
        body_json.get("model").and_then(|v| v.as_str()),
        Some("grok-4-fast-non-reasoning"),
        "request model field must match the configured Grok slug"
    );
    assert!(
        body_json
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "request must opt into streaming"
    );
    assert!(
        body_json.get("messages").is_some(),
        "request must carry OpenAI-style messages array"
    );

    std::env::remove_var(&env_key);
}

// ── Test: Gemini routes through the NATIVE transport (M4 flip) ──
//
// Before M4, Gemini shared the OpenAI-compat transport (POST to
// `…/v1beta/openai/chat/completions` with `Authorization: Bearer …`),
// which silently stripped `inline_data` parts so `vision_analyze` /
// `video_analyze` lost their payloads. After M4 the dispatcher routes
// Gemini to its own native transport (`gemini_transport()`):
//   - URL:  `…/v1beta/models/<model>:streamGenerateContent?alt=sse`
//   - Auth: `x-goog-api-key: <key>` (NOT Bearer — AI Studio rejects
//           Bearer for raw API keys)
//   - Body: Gemini-shape (`contents[].parts[].text`, not OpenAI
//           `messages[].content`)
//   - SSE:  Gemini chunks (`candidates[0].content.parts[].text` plus
//           `finishReason: "STOP"`), not OpenAI `choices[].delta`.
//
// Regressions here typically look like a) 404s from
// `/v1beta/openai/chat/completions` (the dispatcher fell back to the
// compat path), b) 401s from the native API rejecting a Bearer header,
// or c) empty TextDelta events because the OpenAI SSE parser ignores
// Gemini chunks. The native-transport `endpoint()` / `build_headers()`
// / SSE parser unit tests live next to the implementation
// (`llm/gemini.rs`, `llm/provider.rs`); this test pins the
// **end-to-end wire behavior** post-flip.
#[tokio::test]
async fn gemini_streams_via_native_transport() {
    let server = MockServer::start().await;

    // Gemini SSE: one delta + one final chunk with `finishReason: "STOP"`
    // + `usageMetadata`. No `[DONE]` sentinel — Google closes the socket.
    let body = sse_body(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"hi from Gemini"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":4,"totalTokenCount":9}}"#,
    ]);

    // Path-match on the native streaming endpoint. The full URL is
    // `…/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse`;
    // wiremock `path()` matches the bare path, so the `?alt=sse` query
    // param is asserted separately below via `received_requests`.
    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
        ))
        .and(header("x-goog-api-key", "gemini-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_GEMINI_KEY_{n}");
    std::env::set_var(&env_key, "gemini-test-key");
    // BORG_GEMINI_BASE_URL is the documented test/relay override and
    // beats `llm.base_url` — using it here keeps the `compose_gemini_url`
    // path itself in scope (so a future refactor that broke the env
    // override would surface in this test).
    std::env::set_var("BORG_GEMINI_BASE_URL", server.uri());

    let mut config = Config::default();
    config.llm.provider = Some("gemini".into());
    config.llm.api_key_env = env_key.clone();
    config.llm.model = "gemini-2.5-flash".into();
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(
        events.iter().any(|e| e == "text:hi from Gemini"),
        "expected the Gemini delta to surface as a TextDelta event, got {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "usage:5:4:9"),
        "expected usage extraction from usageMetadata, got {events:?}"
    );
    assert_eq!(
        events.last().map(|s| s.as_str()),
        Some("done"),
        "stream must terminate with Done (finishReason: STOP must emit \
         StreamEvent::Done — without it, the agent loop logs 'stream \
         ended unexpectedly' on every Gemini turn), got {events:?}"
    );

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "expected exactly one POST");
    let req = &received[0];

    // Query-string check: streaming must opt into SSE via `?alt=sse`.
    // Without this, `streamGenerateContent` returns a JSON array, not
    // an event stream, and the SSE parser sees no `data:` prefix.
    assert!(
        req.url.as_str().contains("alt=sse"),
        "streaming URL must include `?alt=sse`, got {}",
        req.url
    );

    // Body shape: Gemini-native, not OpenAI-compat. The OpenAI body has
    // `model` + `messages` + `stream:true` at the top level; Gemini has
    // `contents` (no `model` — that's in the URL — and no `stream` field).
    let body_json: serde_json::Value =
        serde_json::from_slice(&req.body).expect("request body is JSON");
    assert!(
        body_json.get("contents").is_some(),
        "Gemini-native body must have `contents` (OpenAI-compat would \
         have had `messages`), got {body_json}"
    );
    assert!(
        body_json.get("messages").is_none(),
        "post-M4 the body must NOT carry OpenAI-compat `messages` (that \
         would mean the dispatcher fell back to the compat path), got {body_json}"
    );
    let contents = body_json["contents"].as_array().expect("contents array");
    assert!(!contents.is_empty(), "contents must include the user turn");
    let first_text = contents[0]
        .pointer("/parts/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        first_text, "ping",
        "user message text must reach the wire as `contents[0].parts[0].text`"
    );

    std::env::remove_var(&env_key);
    std::env::remove_var("BORG_GEMINI_BASE_URL");
}

// ── Test: Azure OpenAI uses `api-key` header, not `Authorization: Bearer` ──
//
// Azure's static-key auth is the **most common operator misconfiguration**
// for this provider: every other OpenAI-compatible host accepts
// `Authorization: Bearer …`, but Azure rejects that header for static keys
// and requires the `api-key` header instead. Entra ID tokens use Bearer.
// This test pins the header path so the
// `Provider::AzureOpenAi` match arm in `build_headers` can't silently
// regress to the OpenAI Bearer path.
//
// The composed URL (`https://{resource}.openai.azure.com/openai/deployments/...`)
// is covered by `compose_azure_openai_url_*` unit tests in `llm/tests.rs`;
// this test verifies the **header bytes hitting the wire** and the SSE
// round-trip via the shared OpenAI-compat transport.
#[tokio::test]
async fn azure_openai_uses_api_key_header_not_bearer() {
    let server = MockServer::start().await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"hi from Azure"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":3,"total_tokens":6}}"#,
        "[DONE]",
    ]);

    // Fail-closed header matching: if `api-key` is missing OR `authorization`
    // is present, no Mock matches → wiremock returns 404 → the streaming
    // call errors out. Either failure mode signals a regression.
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt-4o-prod/chat/completions"))
        .and(header("api-key", "azure-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let env_key = format!("BORG_TEST_AZURE_KEY_{n}");
    std::env::set_var(&env_key, "azure-test-key");

    let mut config = Config::default();
    config.llm.provider = Some("azure-openai".into());
    config.llm.api_key_env = env_key.clone();
    // Override base_url so the request lands on wiremock instead of
    // *.openai.azure.com. With base_url set, the construction-time URL
    // composer skips (verified by `azure_openai_construction_respects_base_url_override`).
    config.llm.base_url = Some(format!(
        "{}/openai/deployments/gpt-4o-prod/chat/completions?api-version=2024-10-21",
        server.uri()
    ));
    config.llm.model = "gpt-4o-prod".into();
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);

    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(
        events.iter().any(|e| e == "text:hi from Azure"),
        "expected the Azure delta to surface as a TextDelta event, got {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "usage:3:3:6"),
        "expected usage extraction, got {events:?}"
    );
    assert_eq!(
        events.last().map(|s| s.as_str()),
        Some("done"),
        "stream must terminate with Done, got {events:?}"
    );

    // The fail-closed `api-key` header matcher above is the proof of the
    // header-shape invariant: if the path was `Authorization: Bearer` the
    // Mock wouldn't have matched and the stream would error out before
    // reaching `events`. Verify that exactly one POST landed so a future
    // retry-loop addition can't quietly hide a malformed first attempt.
    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "expected exactly one POST");

    std::env::remove_var(&env_key);
}

#[tokio::test]
async fn azure_openai_uses_entra_bearer_when_no_api_key_is_configured() {
    let _guard = CLOUD_ENV_LOCK.lock().await;
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/tenant/oauth2/v2.0/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "entra-access-token",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"hi from Entra"},"finish_reason":null}]}"#,
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt-4o-prod/chat/completions"))
        .and(header("Authorization", "Bearer entra-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let missing_env = format!("BORG_TEST_AZURE_MISSING_KEY_{n}");
    std::env::remove_var(&missing_env);
    std::env::set_var("AZURE_TENANT_ID", "tenant");
    std::env::set_var("AZURE_CLIENT_ID", "client");
    std::env::set_var("AZURE_CLIENT_SECRET", "secret");
    std::env::set_var(
        "BORG_AZURE_TOKEN_URL",
        format!("{}/tenant/oauth2/v2.0/token", server.uri()),
    );

    let mut config = Config::default();
    config.llm.provider = Some("azure-openai".into());
    config.llm.api_key_env = missing_env;
    config.llm.base_url = Some(format!(
        "{}/openai/deployments/gpt-4o-prod/chat/completions?api-version=2024-10-21",
        server.uri()
    ));
    config.llm.model = "gpt-4o-prod".into();
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(events.iter().any(|e| e == "text:hi from Entra"));

    std::env::remove_var("AZURE_TENANT_ID");
    std::env::remove_var("AZURE_CLIENT_ID");
    std::env::remove_var("AZURE_CLIENT_SECRET");
    std::env::remove_var("BORG_AZURE_TOKEN_URL");
}

#[tokio::test]
async fn vertex_gemini_uses_gcp_bearer_and_openai_compat_body() {
    let _guard = CLOUD_ENV_LOCK.lock().await;
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(
            "/computeMetadata/v1/instance/service-accounts/default/token",
        ))
        .and(header("Metadata-Flavor", "Google"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "gcp-access-token",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"hi from Vertex Gemini"},"finish_reason":null}]}"#,
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .and(path("/vertex/openai/chat/completions"))
        .and(header("Authorization", "Bearer gcp-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let old_creds = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok();
    std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
    std::env::set_var(
        "BORG_GCP_METADATA_TOKEN_URL",
        format!(
            "{}/computeMetadata/v1/instance/service-accounts/default/token",
            server.uri()
        ),
    );

    let mut config = Config::default();
    config.llm.provider = Some("vertex-ai".into());
    config.llm.model = "google/gemini-2.5-flash".into();
    config.llm.vertex_project = Some("acme-prod".into());
    config.llm.vertex_location = Some("us-central1".into());
    config.llm.base_url = Some(format!("{}/vertex/openai/chat/completions", server.uri()));
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(events.iter().any(|e| e == "text:hi from Vertex Gemini"));
    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    let llm_req = received
        .iter()
        .find(|r| r.url.path() == "/vertex/openai/chat/completions")
        .expect("LLM request recorded");
    let body_json: serde_json::Value =
        serde_json::from_slice(&llm_req.body).expect("request body is JSON");
    assert_eq!(body_json["model"], "gemini-2.5-flash");

    if let Some(old) = old_creds {
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", old);
    }
    std::env::remove_var("BORG_GCP_METADATA_TOKEN_URL");
}

#[tokio::test]
async fn vertex_claude_uses_gcp_bearer_and_vertex_anthropic_body() {
    let _guard = CLOUD_ENV_LOCK.lock().await;
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/metadata/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "gcp-claude-token",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let body = sse_body(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi from Vertex Claude"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/vertex/claude:streamRawPredict"))
        .and(header("Authorization", "Bearer gcp-claude-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let old_creds = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok();
    std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
    std::env::set_var(
        "BORG_GCP_METADATA_TOKEN_URL",
        format!("{}/metadata/token", server.uri()),
    );

    let mut config = Config::default();
    config.llm.provider = Some("vertex-ai".into());
    config.llm.model = "anthropic/claude-sonnet-4".into();
    config.llm.vertex_project = Some("acme-prod".into());
    config.llm.vertex_location = Some("us-central1".into());
    config.llm.base_url = Some(format!("{}/vertex/claude:streamRawPredict", server.uri()));
    config.llm.max_retries = 0;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("ping"), None, tx)
        .await
        .expect("stream_chat");

    let events = collect_events(rx).await;
    assert!(events.iter().any(|e| e == "text:hi from Vertex Claude"));

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    let llm_req = received
        .iter()
        .find(|r| r.url.path() == "/vertex/claude:streamRawPredict")
        .expect("LLM request recorded");
    let body_json: serde_json::Value =
        serde_json::from_slice(&llm_req.body).expect("request body is JSON");
    assert!(
        body_json.get("model").is_none(),
        "Vertex Claude request must not include model in body"
    );
    assert_eq!(body_json["anthropic_version"], "vertex-2023-10-16");

    if let Some(old) = old_creds {
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", old);
    }
    std::env::remove_var("BORG_GCP_METADATA_TOKEN_URL");
}
