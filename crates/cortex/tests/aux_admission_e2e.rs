//! End-to-end proof that `llm.aux_max_concurrent` bounds in-flight *provider
//! requests* from auxiliary clients, and that primary clients are not gated.
//!
//! The unit tests in `llm::aux_admission` cover the semaphore arithmetic; what
//! this file covers is the wiring — that `for_auxiliary` tags the client and
//! that the tag is honoured on the real request path.
//!
//! Concurrency is measured by wall clock rather than by counting inside the
//! mock responder: wiremock invokes `Respond::respond` synchronously, so a
//! blocking sleep there would serialize every request and make the assertion
//! pass no matter what the client did. An async `set_delay` leaves the server
//! genuinely concurrent, and total elapsed time then discriminates cleanly.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::config::Config;
use cortex::llm::LlmClient;
use cortex::security::secrets_resolve::SecretRef;
use cortex::types::Message;

/// Per-request server-side delay. Long enough that serialized and concurrent
/// runs are unambiguously distinguishable.
const DELAY: Duration = Duration::from_millis(200);
const REQUESTS: usize = 4;

async fn delayed_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(DELAY).set_body_json(
            serde_json::json!({
                "id": "c1",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }),
        ))
        .mount(&server)
        .await;
    server
}

fn base_config(server: &MockServer, key_var: &str) -> Config {
    let mut config = Config::default();
    config.llm.provider = Some("custom".to_string());
    config.llm.base_url = Some(format!("{}/chat/completions", server.uri()));
    config.llm.api_key = Some(SecretRef::Env {
        var: key_var.to_string(),
    });
    config.llm.model = "test-model".to_string();
    config
}

/// Fire `REQUESTS` concurrent chats and return how long they all took.
async fn elapsed_for<F, Fut>(build: F) -> Duration
where
    F: Fn() -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = LlmClient> + Send,
{
    let messages = vec![Message::user("hi")];
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..REQUESTS {
        let build = build.clone();
        let messages = messages.clone();
        handles.push(tokio::spawn(async move {
            let client = build().await;
            client.chat(&messages, None).await
        }));
    }
    for h in handles {
        h.await.expect("join").expect("request succeeds");
    }
    start.elapsed()
}

#[tokio::test]
async fn aux_clients_respect_the_concurrency_cap() {
    // SAFETY: a name unique to this test; nothing else reads it.
    unsafe { std::env::set_var("BORG_TEST_AUX_GATE_KEY", "test-key") };
    let server = delayed_server().await;
    let mut config = base_config(&server, "BORG_TEST_AUX_GATE_KEY");
    config.llm.aux_max_concurrent = 1;
    config.llm.aux = Some(cortex::config::AuxLlmConfig {
        model: "test-model".to_string(),
        ..Default::default()
    });

    let elapsed = elapsed_for(move || {
        let config = config.clone();
        async move {
            LlmClient::for_auxiliary(&config)
                .await
                .expect("aux client builds")
        }
    })
    .await;

    // Serialized at limit 1: four 200ms requests cannot finish in under ~800ms.
    // Ungated they overlap and land near 200ms, so the 3× margin is wide.
    assert!(
        elapsed >= DELAY * (REQUESTS as u32 - 1),
        "aux requests must serialize at llm.aux_max_concurrent=1, but {REQUESTS} \
         requests finished in {elapsed:?}"
    );
}

#[tokio::test]
async fn primary_clients_are_not_gated_by_the_aux_limit() {
    // The failure this guards: a fix that gates *every* client would make
    // interactive turns queue behind background work.
    // SAFETY: a name unique to this test; nothing else reads it.
    unsafe { std::env::set_var("BORG_TEST_AUX_PRIMARY_KEY", "test-key") };
    let server = delayed_server().await;
    let mut config = base_config(&server, "BORG_TEST_AUX_PRIMARY_KEY");
    config.llm.aux_max_concurrent = 1;

    let elapsed = elapsed_for(move || {
        let config = config.clone();
        async move {
            LlmClient::new(&config)
                .await
                .expect("primary client builds")
        }
    })
    .await;

    assert!(
        elapsed < DELAY * 2,
        "primary/interactive turns must run concurrently, but {REQUESTS} \
         requests took {elapsed:?}"
    );
}
