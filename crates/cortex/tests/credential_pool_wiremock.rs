//! Integration tests for the credential pool's 429-rotation behavior under
//! the production `LlmClient` failover loop. Each test stands up a wiremock
//! server with key-discriminated responses (matched on the `Authorization`
//! header), points `LlmClient` at it with a multi-key config, and asserts on
//! the stream's events plus the pool's post-request state.
//!
//! These tests share the process-wide `BORG_DATA_DIR` env var because the
//! credential pool persists to `~/.borg/borg.db` via `Config::db_path()`. A
//! module-level `Mutex` serializes them so each gets a clean tempdir.

#![allow(
    clippy::await_holding_lock,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::unwrap_used
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::config::Config;
use cortex::llm::{LlmClient, StreamEvent};
use cortex::secrets_resolve::SecretRef;
use cortex::types::{Message, MessageContent, Role};

/// Process-global serialization for tests that mutate `BORG_DATA_DIR`.
static SERIAL: Mutex<()> = Mutex::new(());

/// Monotonic suffix for per-test env var names — avoids residual values
/// from earlier tests bleeding into later ones inside the same process.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Pin `BORG_DATA_DIR` to a fresh tempdir for the duration of the test.
fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: `SERIAL` above guarantees only one test in this binary mutates
    // env at a time; no other thread reads `BORG_DATA_DIR` concurrently.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    tmp
}

/// Build a two-key `Config` that points LLM requests at the wiremock server.
/// Returns the config plus the plaintext key values so tests can match against
/// them in `header("authorization", "Bearer <k>")` assertions.
fn two_key_config(server_uri: &str) -> (Config, String, String) {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let var_a = format!("BORG_TEST_POOL_KEY_A_{n}");
    let var_b = format!("BORG_TEST_POOL_KEY_B_{n}");
    let key_a = format!("sk-pool-a-{n}");
    let key_b = format!("sk-pool-b-{n}");
    // SAFETY: serialized by SERIAL.
    unsafe {
        std::env::set_var(&var_a, &key_a);
        std::env::set_var(&var_b, &key_b);
    }

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = var_a.clone();
    config.llm.base_url = Some(format!("{server_uri}/v1/chat/completions"));
    // ≥1 so the inner attempt loop can re-pick after a 429.
    config.llm.max_retries = 3;
    config.llm.initial_retry_delay_ms = 1;
    config.llm.api_keys = vec![SecretRef::Env { var: var_a }, SecretRef::Env { var: var_b }];
    config.llm.credential_cooldown_secs = 60;
    (config, key_a, key_b)
}

fn user_message(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: Some(MessageContent::Text(text.to_string())),
        tool_calls: None,
        tool_call_id: None,
        timestamp: None,
    }]
}

fn sse_done(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

async fn collect_events(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<String> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::TextDelta(t) => events.push(format!("text:{t}")),
            StreamEvent::Done => {
                events.push("done".into());
                break;
            }
            StreamEvent::Error(e) => {
                events.push(format!("error:{e}"));
                break;
            }
            _ => {}
        }
    }
    events
}

// ── Test 83: stream survives 429 on first key, succeeds on second ────────

#[tokio::test]
async fn stream_survives_429_then_succeeds_on_second_key() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, key_a, key_b) = two_key_config(&server.uri());

    // Key A → 429. Key B → 200 SSE.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_done("ok"), "text/event-stream"))
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat should succeed via key B");

    let events = collect_events(rx).await;
    assert_eq!(events, vec!["text:ok", "done"]);

    // The pool should now show key A cooled and key B clean.
    let pool = client
        .primary_pool()
        .cloned()
        .expect("primary pool present for multi-key config");
    let entries = pool.entries();
    assert_eq!(entries.len(), 2, "two seeded credentials");
    let cooled_count = entries
        .iter()
        .filter(|e| e.last_error_reset_at_ms.is_some())
        .count();
    assert_eq!(cooled_count, 1, "exactly one entry cooled by the 429");
    // The cooled key is the one that 429'd; the other should be clean
    // (no last_error_reset_at_ms, no last_error_code).
    let clean = entries
        .iter()
        .find(|e| e.last_error_reset_at_ms.is_none() && e.removed_at_ms.is_none())
        .expect("a non-cooled entry");
    assert!(
        clean.use_count > 0,
        "the surviving key should have been picked at least once"
    );
}

// ── Test 84: all-429 surfaces an exhaustion error ─────────────────────────

#[tokio::test]
async fn all_keys_429_surfaces_exhaustion_error() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, _key_a, _key_b) = two_key_config(&server.uri());

    // Every request → 429.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    let res = client.stream_chat(&user_message("hi"), None, tx).await;

    let events = collect_events(rx).await;
    assert!(res.is_err(), "stream_chat must propagate exhaustion");
    let err_event = events
        .iter()
        .find(|e| e.starts_with("error:"))
        .expect("an Error stream event was delivered");
    let lower = err_event.to_ascii_lowercase();
    assert!(
        lower.contains("429")
            || lower.contains("rate")
            || lower.contains("exhausted")
            || lower.contains("retry"),
        "error event must name the failure mode, got: {err_event}"
    );

    // Pool: both credentials should be cooled (non-removed).
    let pool = client
        .primary_pool()
        .cloned()
        .expect("primary pool present");
    let entries = pool.entries();
    assert_eq!(entries.len(), 2);
    let cooled = entries
        .iter()
        .filter(|e| e.last_error_reset_at_ms.is_some() && e.removed_at_ms.is_none())
        .count();
    assert_eq!(cooled, 2, "every key cooled after 429 storm");
}

// ── Test 85: `Retry-After` header overrides configured cooldown ──────────

#[tokio::test]
async fn retry_after_header_overrides_config_cooldown() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, key_a, key_b) = two_key_config(&server.uri());
    assert_eq!(config.llm.credential_cooldown_secs, 60);

    // Key A → 429 with Retry-After: 300 (way larger than config's 60).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "300")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    // Key B → 200 so the stream completes deterministically.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_done("ok"), "text/event-stream"))
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat succeeds via key B");
    let _events = collect_events(rx).await;

    let pool = client
        .primary_pool()
        .cloned()
        .expect("primary pool present");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let next = pool
        .next_recovery_at_ms()
        .expect("a cooldown timestamp was recorded");
    let remaining_secs = (next - now_ms) / 1000;
    // Tight band around the header value (300s) — narrow enough that a
    // regression to the configured cooldown (60s), the body parser's
    // default, or any off-by-factor-of-ten bug fails this assertion.
    // Lower bound trades 10s of slack for test-runner jitter.
    assert!(
        (290..=305).contains(&remaining_secs),
        "Retry-After header value (300s) must drive the cooldown, got {remaining_secs}s"
    );
}
