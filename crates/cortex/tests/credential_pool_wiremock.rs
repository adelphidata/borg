//! Integration tests for the credential pool's 429-rotation behavior under
//! the production `LlmClient` failover loop. Each test stands up a wiremock
//! server with key-discriminated responses (matched on the `Authorization`
//! header), points `LlmClient` at it with a multi-key config, and asserts on
//! the stream's events plus the pool's post-request state.
//!
//! These tests share the process-wide `BORG_HOME` env var because the
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
use cortex::security::secrets_resolve::SecretRef;
use cortex::types::{Message, MessageContent, Role};

/// Process-global serialization for tests that mutate `BORG_HOME`.
static SERIAL: Mutex<()> = Mutex::new(());

/// Monotonic suffix for per-test env var names — avoids residual values
/// from earlier tests bleeding into later ones inside the same process.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Pin `BORG_HOME` to a fresh tempdir for the duration of the test.
fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: `SERIAL` above guarantees only one test in this binary mutates
    // env at a time; no other thread reads `BORG_HOME` concurrently.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
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

/// Build a one-key primary config with a same-provider fallback slot that
/// points at a second wiremock server.
fn one_key_with_fallback_config(primary_uri: &str, fallback_uri: &str) -> (Config, String) {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let var_a = format!("BORG_TEST_POOL_PRIMARY_KEY_{n}");
    let key_a = format!("sk-pool-primary-{n}");
    unsafe {
        std::env::set_var(&var_a, &key_a);
    }

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = var_a.clone();
    config.llm.base_url = Some(format!("{primary_uri}/v1/chat/completions"));
    config.llm.max_retries = 0;
    config.llm.api_keys = vec![SecretRef::Env { var: var_a.clone() }];
    config.llm.credential_cooldown_secs = 60;
    config.llm.fallback.push(cortex::config::LlmFallback {
        provider: "openai".into(),
        model: config.llm.model.clone(),
        api_key: None,
        api_key_env: Some(var_a),
        api_keys: Vec::new(),
        temperature: None,
        max_tokens: None,
        base_url: Some(format!("{fallback_uri}/v1/chat/completions")),
    });
    (config, key_a)
}

fn user_message(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: Some(MessageContent::Text(text.to_string())),
        tool_calls: None,
        tool_call_id: None,
        timestamp: None,
        seq: None,
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

/// A non-streaming OpenAI-shaped chat completion body.
fn chat_completion(text: &str) -> String {
    format!(
        "{{\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":\"{text}\"}},\
         \"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1}}}}"
    )
}

// ── Non-streaming path must use the credential pool too ──────────────────
//
// `chat()` carries the autonomous lanes: scheduled prompt tasks, the heartbeat
// turn, and the pre-compaction flush. It used to retry a single `self.api_key`
// in place — never leasing from the pool, so a 429 was retried on the same key
// until exhaustion and never cooled, rotated, or failed over, and an expired
// OAuth token was never refreshed (only the lease path refreshes).

#[tokio::test]
async fn chat_survives_429_then_succeeds_on_second_key() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, key_a, key_b) = two_key_config(&server.uri());

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chat_completion("ok"), "application/json"),
        )
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    let reply = client
        .chat(&user_message("hi"), None)
        .await
        .expect("chat should rotate off the 429'd key and succeed via key B");
    assert_eq!(reply.text_content(), Some("ok"));

    let pool = client
        .primary_pool()
        .cloned()
        .expect("primary pool present for multi-key config");
    let entries = pool.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.last_error_reset_at_ms.is_some())
            .count(),
        1,
        "the 429 must cool exactly one credential — proof the pool saw the failure"
    );
}

#[tokio::test]
async fn a_benched_credential_records_the_http_status_that_benched_it() {
    // `borg auth list` shows `last_error_code` so an operator can see *why* a
    // key is cooling. Production always passed `None`, so the column was
    // always blank — the status was known in `send_request` but dropped on the
    // way to `report_failure`. `LlmError` now carries it.
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, key_a, key_b) = two_key_config(&server.uri());

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chat_completion("ok"), "application/json"),
        )
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    client
        .chat(&user_message("hi"), None)
        .await
        .expect("second key serves the request");

    let pool = client.primary_pool().cloned().expect("primary pool");
    let benched: Vec<_> = pool
        .entries()
        .into_iter()
        .filter(|e| e.last_error_reset_at_ms.is_some())
        .collect();
    assert_eq!(
        benched.len(),
        1,
        "the 429 must bench exactly one credential"
    );
    assert_eq!(
        benched[0].last_error_code,
        Some(429),
        "the benched credential must record the status that benched it"
    );
}

#[tokio::test]
async fn chat_falls_back_to_the_next_provider_when_the_pool_is_exhausted() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    let (config, key_a) = one_key_with_fallback_config(&primary.uri(), &fallback.uri());

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "300")
                .set_body_string("rate limited"),
        )
        .expect(1)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(chat_completion("fallback-ok"), "application/json"),
        )
        .expect(1)
        .mount(&fallback)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    let reply = client
        .chat(&user_message("hi"), None)
        .await
        .expect("chat should reach the fallback provider");
    assert_eq!(reply.text_content(), Some("fallback-ok"));
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

    let mut client = LlmClient::new(&config).await.expect("client");
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

    let mut client = LlmClient::new(&config).await.expect("client");
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

    let mut client = LlmClient::new(&config).await.expect("client");
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

// ── Test 86: single-key pool falls through to fallback provider ──────────

#[tokio::test]
async fn single_key_pool_429_uses_fallback_provider() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    let (config, key_a) = one_key_with_fallback_config(&primary.uri(), &fallback.uri());

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "300")
                .set_body_string("rate limited"),
        )
        .expect(1)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_done("fallback-ok"), "text/event-stream"),
        )
        .expect(1)
        .mount(&fallback)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("stream_chat should succeed via fallback provider");

    let events = collect_events(rx).await;
    assert_eq!(events, vec!["text:fallback-ok", "done"]);
}

// ── Test 87: snapshot captured from a successful response cools the pool ──
//
// Positive guard for the success-path branch of the proactive layer. Without
// this, the layer would only fire when the prior request was a 429 — which
// already triggered the reactive cooldown anyway, defeating the point.

#[tokio::test]
async fn rate_limit_headers_from_success_response_skip_near_empty_key() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (mut config, key_a, key_b) = two_key_config(&server.uri());
    // 5% threshold, 5s min window — defaults, but pin them in the test for
    // readability of what we're asserting against.
    config.llm.proactive_rate_limit_threshold_pct = 5;
    config.llm.proactive_rate_limit_min_window_secs = 5;

    // Key A: 200 OK with near-empty bucket headers (0.1% remaining,
    // resets in 120s — well past the 5s min window).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "1000")
                .insert_header("x-ratelimit-remaining-requests", "1")
                .insert_header("x-ratelimit-reset-requests", "120s")
                .set_body_raw(sse_done("ok-a"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Key B: 200 OK with healthy bucket. Must receive the second request.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "1000")
                .insert_header("x-ratelimit-remaining-requests", "999")
                .insert_header("x-ratelimit-reset-requests", "60s")
                .set_body_raw(sse_done("ok-b"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");

    // First request: FillFirst picks A. A succeeds and reports a near-empty
    // bucket via its response headers.
    let (tx1, rx1) = mpsc::channel(64);
    client
        .stream_chat(&user_message("first"), None, tx1)
        .await
        .expect("first request via A succeeds");
    let _ = collect_events(rx1).await;

    // Verify the snapshot landed specifically on A (not just "some credential"):
    // FillFirst picks lowest priority, seed order gives A priority 0, so the
    // only entry with use_count > 0 after request 1 is A. Pinning the spec
    // here means a routing regression (e.g. picking B first) fails this test
    // instead of silently passing on B's healthy snapshot.
    let pool = client.primary_pool().cloned().expect("pool");
    let entries = pool.entries();
    let a = entries
        .iter()
        .find(|c| c.use_count > 0)
        .expect("the credential that ran request 1");
    let b = entries
        .iter()
        .find(|c| c.use_count == 0)
        .expect("the credential that has not yet been used");
    let a_req = a
        .rate_limit
        .as_ref()
        .and_then(|s| s.requests)
        .expect("A has a captured snapshot");
    assert_eq!(a_req.remaining, 1, "A's snapshot reflects A's headers");
    assert_eq!(a_req.limit, 1000);
    assert!(
        b.rate_limit.is_none(),
        "B has not been hit yet, so it must not carry a snapshot"
    );

    // Second request: proactive avoidance should skip A (0.1% < 5%) and
    // route to B. Wiremock's `.expect(1)` on B's mock will fail if B is not
    // hit exactly once.
    let (tx2, rx2) = mpsc::channel(64);
    client
        .stream_chat(&user_message("second"), None, tx2)
        .await
        .expect("second request routes to B");
    let _ = collect_events(rx2).await;

    // Drop client so wiremock's `.expect()` assertions are evaluated on
    // server drop.
    drop(client);
}

// ── Test 88: proactive avoidance disabled (threshold=0) keeps using A ────

#[tokio::test]
async fn proactive_disabled_keeps_using_near_empty_credential() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (mut config, key_a, _key_b) = two_key_config(&server.uri());
    // Disable proactive avoidance.
    config.llm.proactive_rate_limit_threshold_pct = 0;

    // Key A returns 200 with near-empty bucket on EVERY request. We expect
    // both requests to land on A because the feature is off.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "1000")
                .insert_header("x-ratelimit-remaining-requests", "1")
                .insert_header("x-ratelimit-reset-requests", "120s")
                .set_body_raw(sse_done("ok"), "text/event-stream"),
        )
        .expect(2)
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    for _ in 0..2 {
        let (tx, rx) = mpsc::channel(64);
        client
            .stream_chat(&user_message("ping"), None, tx)
            .await
            .expect("request succeeds via A");
        let _ = collect_events(rx).await;
    }
    drop(client);
}

// ── Test 89: within the min-window, wait rather than rotate ──────────────

#[tokio::test]
async fn proactive_within_min_window_does_not_rotate() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (mut config, key_a, _key_b) = two_key_config(&server.uri());
    config.llm.proactive_rate_limit_threshold_pct = 5;
    // 10s min window — bucket resets in 2s, so the layer should let A keep
    // serving rather than rotate to B.
    config.llm.proactive_rate_limit_min_window_secs = 10;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit-requests", "1000")
                .insert_header("x-ratelimit-remaining-requests", "1")
                .insert_header("x-ratelimit-reset-requests", "2s")
                .set_body_raw(sse_done("ok"), "text/event-stream"),
        )
        .expect(2)
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    for _ in 0..2 {
        let (tx, rx) = mpsc::channel(64);
        client
            .stream_chat(&user_message("ping"), None, tx)
            .await
            .expect("request succeeds via A");
        let _ = collect_events(rx).await;
    }
    drop(client);
}

// ── Test 90: a 429 response also records its rate-limit snapshot ─────────

#[tokio::test]
async fn rate_limit_snapshot_captured_from_429_response() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let (mut config, key_a, key_b) = two_key_config(&server.uri());
    config.llm.proactive_rate_limit_threshold_pct = 5;
    config.llm.proactive_rate_limit_min_window_secs = 5;

    // Key A returns 429 with bucket headers attached. The pool should both:
    //   1. Cool A reactively (existing behavior) via Retry-After.
    //   2. Capture a rate-limit snapshot from the same response so future
    //      sessions / restarts can act on the headers.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_a}").as_str()))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .insert_header("x-ratelimit-limit-requests", "1000")
                .insert_header("x-ratelimit-remaining-requests", "0")
                .insert_header("x-ratelimit-reset-requests", "60s")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    // Key B handles the retry.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {key_b}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_done("ok"), "text/event-stream"))
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    let (tx, rx) = mpsc::channel(64);
    client
        .stream_chat(&user_message("hi"), None, tx)
        .await
        .expect("succeeds via B");
    let _ = collect_events(rx).await;

    let pool = client.primary_pool().cloned().expect("pool");
    let entries = pool.entries();
    let cooled = entries
        .iter()
        .find(|e| e.last_error_reset_at_ms.is_some())
        .expect("A cooled by 429");
    let snap = cooled
        .rate_limit
        .as_ref()
        .expect("snapshot captured from 429 headers");
    let req = snap.requests.expect("requests bucket parsed");
    assert_eq!(req.remaining, 0);
    assert_eq!(req.limit, 1000);
}

// ── 401 grace: force one OAuth refresh before tombstoning ────────────────

use cortex::db::Database;
use cortex::llm::credential_pool::{CredentialPool, CredentialStrategy, PoolLoadConfig};
use cortex::llm::llm_error::FailoverReason;
use cortex::llm::provider::Provider;

async fn oauth_pool(
    access: &str,
    refresh: &str,
    expires_at_ms: i64,
) -> (
    std::sync::Arc<CredentialPool>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":{expires_at_ms}}}}}"#
        ),
    )
    .unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pool.db");
    let db = Database::open_at(&db_path).await.unwrap();
    let pool = CredentialPool::load(
        &db,
        &db_path,
        Provider::ClaudeCodeOAuth,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![],
            env_vars: vec![],
            oauth_home: Some(home.path().to_path_buf()),
        },
    )
    .await
    .unwrap();
    (pool, home, db_dir)
}

#[tokio::test]
async fn auth_failure_on_refreshable_oauth_forces_refresh_instead_of_removal() {
    // A 401 can mean the access token is stale-but-unexpired (rotated by
    // another device) while the refresh token is still good. The first Auth
    // failure must spend a one-shot grace — force-expire the token so the
    // next lease refreshes it — instead of permanently removing a fixable
    // credential.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"access_token":"refreshed-access","refresh_token":"refreshed-r","expires_in":3600}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    // Expiry far in the future — the provider rejected a "valid" token.
    let (pool, _home, _db) = oauth_pool("stale-but-unexpired", "r1", 9_999_999_999_000).await;
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));
    let id = pool.entries()[0].id.clone();

    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, Some("401"));
    assert!(
        !pool.entries()[0].is_removed(),
        "first 401 on a refreshable OAuth credential must not tombstone"
    );

    // The forced expiry makes the next lease run the real refresh path.
    let lease = pool.acquire_lease_refreshing(None).await.unwrap();
    assert_eq!(
        lease.credential().unwrap().key,
        "refreshed-access",
        "401 grace must lead to a refreshed access token"
    );
}

#[tokio::test]
async fn oauth_refresh_locks_do_not_accumulate() {
    // The per-credential refresh-lock map inserted an entry per credential and
    // never removed one — including for credentials later tombstoned by a
    // 401/403. Unbounded in principle on a long-lived daemon. It is now swept
    // on each use, retaining only entries with a refresh actually in flight.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"fresh","refresh_token":"r2","expires_in":3600}"#,
            ),
        )
        .mount(&server)
        .await;

    // Already-expired access token, so each lease drives the refresh path.
    let (pool, _home, _db) = oauth_pool("stale", "r", 1_000).await;
    pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

    for _ in 0..5 {
        let lease = pool
            .acquire_lease_refreshing(None)
            .await
            .expect("a credential is available");
        drop(lease);
        assert!(
            pool.refresh_lock_count() <= 1,
            "refresh-lock map must stay bounded across repeated refreshes; got {}",
            pool.refresh_lock_count()
        );
    }
}

#[tokio::test]
async fn second_auth_failure_after_grace_tombstones() {
    // The grace is one-shot per incident: if the refreshed token still 401s,
    // the credential is dead — removed exactly as before.
    let (pool, _home, _db) = oauth_pool("a", "r", 9_999_999_999_000).await;
    let id = pool.entries()[0].id.clone();

    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    assert!(!pool.entries()[0].is_removed(), "grace on first 401");
    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    assert!(
        pool.entries()[0].is_removed(),
        "second 401 in the same incident must tombstone"
    );
}

#[tokio::test]
async fn success_between_auth_incidents_rearms_the_grace() {
    let (pool, _home, _db) = oauth_pool("a", "r", 9_999_999_999_000).await;
    let id = pool.entries()[0].id.clone();
    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    pool.report_success(&id);
    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    assert!(
        !pool.entries()[0].is_removed(),
        "a success between auth incidents re-arms the one-shot grace"
    );
}

#[tokio::test]
async fn auth_failure_still_tombstones_api_keys_and_permanent_denials() {
    // The grace is scoped to refreshable OAuth credentials: a 401 on a plain
    // API key means the key is bad (nothing to refresh), and a 403
    // (AuthPermanent) means the account is denied — both remove immediately.
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _dir = isolated_data_dir();
    let server = MockServer::start().await;
    let (config, _key_a, _key_b) = two_key_config(&server.uri());
    let client = LlmClient::new(&config).await.expect("client");
    let pool = client.primary_pool().cloned().expect("pool");
    let id = pool.entries()[0].id.clone();
    pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
    assert!(
        pool.entries()
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .is_removed(),
        "401 on an API key must remove it immediately"
    );

    let (pool2, _home, _db) = oauth_pool("a", "r", 9_999_999_999_000).await;
    let id2 = pool2.entries()[0].id.clone();
    pool2.report_failure(&id2, FailoverReason::AuthPermanent, Some(403), None, None);
    assert!(
        pool2.entries()[0].is_removed(),
        "403 must remove even a refreshable OAuth credential"
    );
}

// ── cooldown derivation: billing vs. transient throttle ───────────────────

/// Deterministic clock so cooldown arithmetic is exact rather than racing
/// wall time.
#[derive(Debug)]
struct FixedClock(std::sync::atomic::AtomicI64);

use cortex::llm::credential_pool::Clock;

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Pool with two config keys, a 60s base cooldown and a 1h billing bench.
async fn cooldown_pool() -> (
    std::sync::Arc<CredentialPool>,
    std::sync::Arc<FixedClock>,
    tempfile::TempDir,
) {
    let clock = std::sync::Arc::new(FixedClock(std::sync::atomic::AtomicI64::new(
        1_000_000_000_000,
    )));
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pool.db");
    let db = Database::open_at(&db_path).await.unwrap();
    let pool = CredentialPool::load_with_clock(
        &db,
        &db_path,
        Provider::Anthropic,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![
                cortex::llm::credential_pool::ConfigEntry {
                    key: "sk-ant-key-one".to_string(),
                    label: Some("one".to_string()),
                    config_name: "llm.api_keys[0]".to_string(),
                },
                cortex::llm::credential_pool::ConfigEntry {
                    key: "sk-ant-key-two".to_string(),
                    label: Some("two".to_string()),
                    config_name: "llm.api_keys[1]".to_string(),
                },
            ],
            env_vars: vec![],
            oauth_home: None,
        },
        clock.clone(),
    )
    .await
    .unwrap();
    (pool, clock, db_dir)
}

/// Cooldown length the pool just applied to `id`, in seconds.
fn cooldown_len_secs(pool: &CredentialPool, id: &str, now_ms: i64) -> i64 {
    let e = pool
        .entries()
        .into_iter()
        .find(|c| c.id == id)
        .expect("credential present");
    (e.last_error_reset_at_ms.expect("benched") - now_ms) / 1000
}

#[tokio::test]
async fn billing_failure_benches_credential_far_longer_than_rate_limit() {
    // A spent account cannot recover without human action. Sharing the 60s
    // throttle cooldown meant retrying it every minute forever.
    let (pool, clock, _dir) = cooldown_pool().await;
    let now = clock.now_ms();
    let ids: Vec<String> = pool.entries().into_iter().map(|c| c.id).collect();

    pool.report_failure(&ids[0], FailoverReason::Billing, Some(402), None, None);
    pool.report_failure(&ids[1], FailoverReason::RateLimit, Some(429), None, None);

    assert_eq!(cooldown_len_secs(&pool, &ids[0], now), 3600);
    assert_eq!(cooldown_len_secs(&pool, &ids[1], now), 60);
}

#[tokio::test]
async fn consecutive_rate_limits_escalate_geometrically() {
    // 60s base doubling per consecutive throttle, capped at 6 shifts.
    let (pool, clock, _dir) = cooldown_pool().await;
    let id = pool.entries()[0].id.clone();
    let expected = [60_i64, 120, 240, 480, 960, 1920, 3840, 3840];

    for (attempt, want) in expected.iter().enumerate() {
        // Advance past the previous bench so extend-never-shorten doesn't
        // mask the newly derived duration.
        let now = clock.now_ms() + (want + 1) * 1000;
        clock.0.store(now, Ordering::SeqCst);
        pool.report_failure(&id, FailoverReason::RateLimit, Some(429), None, None);
        assert_eq!(
            cooldown_len_secs(&pool, &id, now),
            *want,
            "attempt {} should bench for {want}s",
            attempt + 1
        );
    }
}

#[tokio::test]
async fn success_resets_rate_limit_escalation() {
    let (pool, clock, _dir) = cooldown_pool().await;
    let id = pool.entries()[0].id.clone();

    for _ in 0..3 {
        let now = clock.now_ms() + 10_000_000;
        clock.0.store(now, Ordering::SeqCst);
        pool.report_failure(&id, FailoverReason::RateLimit, Some(429), None, None);
    }
    pool.report_success(&id);

    let now = clock.now_ms() + 10_000_000;
    clock.0.store(now, Ordering::SeqCst);
    pool.report_failure(&id, FailoverReason::RateLimit, Some(429), None, None);
    assert_eq!(
        cooldown_len_secs(&pool, &id, now),
        60,
        "a completed request must return the key to the base cooldown"
    );
}

#[tokio::test]
async fn provider_retry_after_wins_over_escalated_default() {
    // The provider knows its own bucket. A hint must override the escalated
    // default — but must still not *shorten* an existing longer bench.
    let (pool, clock, _dir) = cooldown_pool().await;
    let ids: Vec<String> = pool.entries().into_iter().map(|c| c.id).collect();

    // Escalate key one to a 480s default, then hand it a 30s hint on a fresh
    // window: the hint wins.
    for _ in 0..4 {
        let now = clock.now_ms() + 10_000_000;
        clock.0.store(now, Ordering::SeqCst);
        pool.report_failure(&ids[0], FailoverReason::RateLimit, Some(429), None, None);
    }
    let now = clock.now_ms() + 10_000_000;
    clock.0.store(now, Ordering::SeqCst);
    pool.report_failure(
        &ids[0],
        FailoverReason::RateLimit,
        Some(429),
        Some(std::time::Duration::from_secs(30)),
        None,
    );
    assert_eq!(cooldown_len_secs(&pool, &ids[0], now), 30);

    // Extend-never-shorten still holds: a short hint arriving while a longer
    // bench is live must not release the credential early.
    pool.report_failure(
        &ids[1],
        FailoverReason::Billing,
        Some(402),
        Some(std::time::Duration::from_secs(7200)),
        None,
    );
    let long = cooldown_len_secs(&pool, &ids[1], now);
    pool.report_failure(
        &ids[1],
        FailoverReason::RateLimit,
        Some(429),
        Some(std::time::Duration::from_secs(5)),
        None,
    );
    assert_eq!(
        cooldown_len_secs(&pool, &ids[1], now),
        long,
        "a 5s hint must not shorten a live 2h bench"
    );
}

#[tokio::test]
async fn billing_cooldown_is_clamped_by_the_hard_ceiling() {
    // Guards against an absurd operator value benching a key past the 24h cap.
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pool.db");
    let db = Database::open_at(&db_path).await.unwrap();
    let clock = std::sync::Arc::new(FixedClock(std::sync::atomic::AtomicI64::new(
        1_000_000_000_000,
    )));
    let pool = CredentialPool::load_with_clock(
        &db,
        &db_path,
        Provider::Anthropic,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: u64::MAX,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![cortex::llm::credential_pool::ConfigEntry {
                key: "sk-ant-only".to_string(),
                label: None,
                config_name: "llm.api_keys[0]".to_string(),
            }],
            env_vars: vec![],
            oauth_home: None,
        },
        clock.clone(),
    )
    .await
    .unwrap();

    let now = clock.now_ms();
    let id = pool.entries()[0].id.clone();
    pool.report_failure(&id, FailoverReason::Billing, Some(402), None, None);
    assert_eq!(cooldown_len_secs(&pool, &id, now), 24 * 60 * 60);
}

// ── `llm.credential_strategy` must actually influence lease selection ─────
//
// The strategy was read only by `pick()`, which has no production call site.
// The request path uses `acquire_lease`, which selected purely by
// (active_leases, priority) — so `round_robin`, `least_used`, and `random`
// all behaved as `fill_first` and the setting was inert config.

/// A three-key pool under the given strategy.
async fn strategy_pool(
    strategy: CredentialStrategy,
) -> (std::sync::Arc<CredentialPool>, tempfile::TempDir) {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pool.db");
    let db = Database::open_at(&db_path).await.unwrap();
    let entries = (0..3)
        .map(|i| cortex::llm::credential_pool::ConfigEntry {
            key: format!("sk-ant-key-{i}"),
            label: Some(format!("k{i}")),
            config_name: format!("llm.api_keys[{i}]"),
        })
        .collect();
    let pool = CredentialPool::load(
        &db,
        &db_path,
        Provider::Anthropic,
        PoolLoadConfig {
            strategy,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: entries,
            env_vars: vec![],
            oauth_home: None,
        },
    )
    .await
    .unwrap();
    (pool, db_dir)
}

/// Ids handed out by `n` sequential acquire-then-drop cycles.
fn sequential_lease_ids(pool: &CredentialPool, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let lease = pool.acquire_lease(None).expect("a credential is available");
            lease.id().to_string()
        })
        .collect()
}

#[tokio::test]
async fn fill_first_keeps_using_the_same_credential() {
    let (pool, _dir) = strategy_pool(CredentialStrategy::FillFirst).await;
    let ids = sequential_lease_ids(&pool, 6);
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        distinct.len(),
        1,
        "fill_first must stay on one credential while it is healthy; got {ids:?}"
    );
}

#[tokio::test]
async fn round_robin_cycles_across_every_credential() {
    let (pool, _dir) = strategy_pool(CredentialStrategy::RoundRobin).await;
    let ids = sequential_lease_ids(&pool, 6);
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        distinct.len(),
        3,
        "round_robin must reach all three credentials; got {ids:?}"
    );
}

#[tokio::test]
async fn least_used_spreads_load_evenly() {
    let (pool, _dir) = strategy_pool(CredentialStrategy::LeastUsed).await;
    let ids = sequential_lease_ids(&pool, 6);
    let mut counts: std::collections::HashMap<&String, usize> = std::collections::HashMap::new();
    for id in &ids {
        *counts.entry(id).or_default() += 1;
    }
    assert_eq!(counts.len(), 3, "all three must be used; got {ids:?}");
    assert!(
        counts.values().all(|&c| c == 2),
        "least_used must balance 6 leases evenly over 3 keys; got {counts:?}"
    );
}

/// Pool with exactly one config key, a 60s base cooldown and a 1h billing bench.
async fn sole_credential_pool() -> (
    std::sync::Arc<CredentialPool>,
    std::sync::Arc<FixedClock>,
    tempfile::TempDir,
) {
    let clock = std::sync::Arc::new(FixedClock(std::sync::atomic::AtomicI64::new(
        1_000_000_000_000,
    )));
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("pool.db");
    let db = Database::open_at(&db_path).await.unwrap();
    let pool = CredentialPool::load_with_clock(
        &db,
        &db_path,
        Provider::Anthropic,
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            billing_cooldown_secs: 3600,
            max_concurrent_per_credential: 4,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![cortex::llm::credential_pool::ConfigEntry {
                key: "sk-ant-only-key".to_string(),
                label: Some("only".to_string()),
                config_name: "llm.api_keys[0]".to_string(),
            }],
            env_vars: vec![],
            oauth_home: None,
        },
        clock.clone(),
    )
    .await
    .unwrap();
    (pool, clock, db_dir)
}

#[tokio::test]
async fn sole_credential_never_escalates_past_the_base_cooldown() {
    // Geometric escalation is a rotation policy: it keeps a capped key from
    // being re-probed ahead of healthy siblings. A one-key pool has no
    // siblings, so escalating only benches the single credential the agent
    // has — up to 64 minutes of hard downtime for what may be a one-second
    // burst. The multi-key pool must keep escalating; this one must not.
    let (pool, clock, _dir) = sole_credential_pool().await;
    let id = pool.entries()[0].id.clone();

    for attempt in 1..=8 {
        let now = clock.now_ms() + 61_000;
        clock.0.store(now, Ordering::SeqCst);
        pool.report_failure(&id, FailoverReason::RateLimit, Some(429), None, None);
        assert_eq!(
            cooldown_len_secs(&pool, &id, now),
            60,
            "throttle {attempt} on a sole credential must stay at the base cooldown"
        );
    }
}

#[tokio::test]
async fn sole_credential_still_honors_a_provider_retry_after() {
    // The provider knows its own bucket. Suppressing escalation must not
    // suppress a longer hint the provider actually sent.
    let (pool, clock, _dir) = sole_credential_pool().await;
    let now = clock.now_ms();
    let id = pool.entries()[0].id.clone();

    pool.report_failure(
        &id,
        FailoverReason::RateLimit,
        Some(429),
        Some(std::time::Duration::from_secs(900)),
        None,
    );
    assert_eq!(cooldown_len_secs(&pool, &id, now), 900);
}

#[tokio::test]
async fn sole_credential_billing_bench_is_untouched_by_the_escalation_change() {
    // Only the RateLimit arm was narrowed. A spent account still gets the full
    // billing bench even when it is the only key — retrying it every 60s is the
    // exact defect that bench exists to prevent.
    let (pool, clock, _dir) = sole_credential_pool().await;
    let now = clock.now_ms();
    let id = pool.entries()[0].id.clone();

    pool.report_failure(&id, FailoverReason::Billing, Some(402), None, None);
    assert_eq!(cooldown_len_secs(&pool, &id, now), 3600);
}

#[tokio::test]
async fn salvage_offers_a_throttled_credential_but_refuses_a_spent_one() {
    // `pick_force` is the last gasp before the turn dies with "pool exhausted".
    // A rate-limit bench is a window that expires, so trying it costs at most
    // one request. Billing and permanent-auth benches need a human, so
    // salvaging them would just re-create the every-60s hammering.
    let (pool, clock, _dir) = sole_credential_pool().await;
    let id = pool.entries()[0].id.clone();

    pool.report_failure(&id, FailoverReason::RateLimit, Some(429), None, None);
    assert_eq!(
        pool.pick_force().map(|c| c.id),
        Some(id.clone()),
        "a throttled credential is worth one salvage attempt"
    );

    clock.0.store(clock.now_ms() + 61_000, Ordering::SeqCst);
    pool.report_failure(&id, FailoverReason::Billing, Some(402), None, None);
    assert!(
        pool.pick_force().is_none(),
        "a spent account must not be salvaged"
    );
}

#[tokio::test]
async fn salvage_prefers_the_soonest_recovering_salvageable_key() {
    // With a mix, the gate must filter rather than abort: the billing-benched
    // key is skipped even though it would sort first by reset time.
    let (pool, clock, _dir) = cooldown_pool().await;
    let ids: Vec<String> = pool.entries().into_iter().map(|c| c.id).collect();

    // Bench the first for billing with a short provider hint so it would win
    // on reset time, and the second for an ordinary throttle.
    pool.report_failure(
        &ids[0],
        FailoverReason::Billing,
        Some(402),
        Some(std::time::Duration::from_secs(1)),
        None,
    );
    pool.report_failure(&ids[1], FailoverReason::RateLimit, Some(429), None, None);
    let _ = clock;

    assert_eq!(
        pool.pick_force().map(|c| c.id),
        Some(ids[1].clone()),
        "the billing-benched key sorts first but must be skipped"
    );
}

/// One key and no fallback slot — the shape of a default single-key install.
/// `max_retries = 1` so the throttled first attempt benches the key and the
/// second attempt reaches the salvage path with an empty pool and nowhere to
/// fail over to.
fn sole_key_no_fallback_config(server_uri: &str) -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let var = format!("BORG_TEST_SOLE_KEY_{n}");
    // SAFETY: serialized by SERIAL.
    unsafe {
        std::env::set_var(&var, format!("sk-sole-{n}"));
    }

    let mut config = Config::default();
    config.llm.provider = Some("openai".into());
    config.llm.api_key_env = var.clone();
    config.llm.base_url = Some(format!("{server_uri}/v1/chat/completions"));
    config.llm.max_retries = 1;
    config.llm.initial_retry_delay_ms = 1;
    config.llm.api_keys = vec![SecretRef::Env { var }];
    config.llm.credential_cooldown_secs = 60;
    config
}

#[tokio::test]
async fn sole_key_install_salvages_its_only_credential_instead_of_going_dark() {
    // A single-key install has no sibling to rotate to and no fallback to fail
    // over to, so a burst throttle used to end the turn outright: the key was
    // benched for the cooldown and `acquire_lease` then refused it. The user
    // saw "credential pool exhausted" for a throttle that had already cleared.
    //
    // With the last-gasp salvage the loop tries the cooling key once more,
    // which is enough to ride out a transient burst.
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _tmp = isolated_data_dir();
    let server = MockServer::start().await;
    let config = sole_key_no_fallback_config(&server.uri());

    // First request throttles, every later one succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_done("recovered")),
        )
        .mount(&server)
        .await;

    let mut client = LlmClient::new(&config).await.expect("client");
    let (tx, rx) = mpsc::channel(64);
    let res = client.stream_chat(&user_message("hi"), None, tx).await;
    let events = collect_events(rx).await;

    assert!(
        res.is_ok(),
        "the salvage attempt must carry the turn: {res:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("recovered")),
        "the recovered answer must reach the caller, got: {events:?}"
    );
}

// ── operator surface: per-credential reset + priority reorder ─────────────

/// Id of the entry labelled `label`.
fn id_of(pool: &CredentialPool, label: &str) -> String {
    pool.entries()
        .into_iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("credential `{label}` present"))
        .id
}

#[tokio::test]
async fn reset_cooldown_for_leaves_sibling_cooldowns_and_strikes_intact() {
    // `borg auth reset <provider> <target>` must be surgical: clearing one
    // benched key must not hand a still-throttled sibling back to the
    // rotation, and must not wipe the sibling's escalation history.
    let (pool, clock, _dir) = cooldown_pool().await;
    let one = id_of(&pool, "one");
    let two = id_of(&pool, "two");
    let now = clock.now_ms();
    for _ in 0..2 {
        pool.report_failure(&one, FailoverReason::RateLimit, Some(429), None, None);
        pool.report_failure(&two, FailoverReason::RateLimit, Some(429), None, None);
    }
    assert!(pool.entries().iter().all(|c| c.is_cooling(now)));

    let cleared = pool.reset_cooldown_for("one").expect("target resolves");
    assert_eq!(
        cleared, 1,
        "the benched target counts as one cleared cooldown"
    );

    let entries = pool.entries();
    let one_e = entries.iter().find(|c| c.id == one).unwrap();
    let two_e = entries.iter().find(|c| c.id == two).unwrap();
    assert!(
        one_e.is_available(now),
        "the target must be back in rotation"
    );
    assert_eq!(
        one_e.consecutive_rate_limits, 0,
        "the target starts a fresh strike window"
    );
    assert!(
        two_e.is_cooling(now),
        "the sibling must still be on cooldown"
    );
    assert_eq!(
        two_e.consecutive_rate_limits, 2,
        "the sibling's strike count must survive a targeted reset"
    );
    assert_eq!(
        pool.reset_cooldown_for(&one).expect("id resolves"),
        0,
        "a second reset of a healthy entry clears nothing"
    );
}

#[tokio::test]
async fn reset_cooldown_for_unknown_target_names_the_valid_targets() {
    let (pool, _clock, _dir) = cooldown_pool().await;
    let err = pool
        .reset_cooldown_for("nope")
        .expect_err("an unknown target must not resolve");
    let msg = err.to_string();
    assert!(msg.contains("nope"), "error names the bad target: {msg}");
    assert!(
        msg.contains("1:one") && msg.contains("2:two"),
        "error lists index:label of every live entry: {msg}"
    );
}

#[tokio::test]
async fn set_priority_moves_entry_and_fill_first_follows() {
    // Under fill_first the lowest priority is always leased. Promoting k2 to
    // position 1 must make it the next lease, and the pool must renumber
    // contiguously so `borg auth list` never shows gaps.
    let (pool, _dir) = strategy_pool(CredentialStrategy::FillFirst).await;
    assert_eq!(sequential_lease_ids(&pool, 1), vec![id_of(&pool, "k0")]);

    let reordered = pool.set_priority("k2", 1).expect("k2 resolves");
    let labels: Vec<&str> = reordered.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["k2", "k0", "k1"]);
    let priorities: Vec<u32> = reordered.iter().map(|c| c.priority).collect();
    assert_eq!(
        priorities,
        [0, 1, 2],
        "renumbering must be contiguous from 0"
    );

    assert_eq!(
        sequential_lease_ids(&pool, 3),
        vec![id_of(&pool, "k2"); 3],
        "fill_first must now lease the promoted credential"
    );
    // The pool's index order follows priority so numeric targets stay honest.
    assert_eq!(pool.resolve_target("1").unwrap().1.label, "k2");

    // Demote to the back by index; the middle entries shift up.
    let reordered = pool.set_priority("1", 3).expect("index resolves");
    let labels: Vec<&str> = reordered.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["k0", "k1", "k2"]);
    let priorities: Vec<u32> = reordered.iter().map(|c| c.priority).collect();
    assert_eq!(priorities, [0, 1, 2]);
}

#[tokio::test]
async fn set_priority_rejects_bad_targets_and_positions() {
    let (pool, _dir) = strategy_pool(CredentialStrategy::FillFirst).await;
    let err = pool.set_priority("k9", 1).expect_err("unknown label");
    assert!(err.to_string().contains("valid targets"), "{err}");
    for bad in [0usize, 4] {
        let err = pool
            .set_priority("k1", bad)
            .expect_err("out-of-range position must be rejected");
        assert!(
            err.to_string().contains("between 1 and 3"),
            "position {bad}: error must state the valid range: {err}"
        );
    }
    // Order must be untouched after the rejected calls.
    let labels: Vec<String> = pool.entries().into_iter().map(|c| c.label).collect();
    assert_eq!(labels, ["k0", "k1", "k2"]);
}
