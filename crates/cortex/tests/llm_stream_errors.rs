//! A text-only streamed call must fail when the provider stops a response
//! with an error or a block, even though the stream then closes normally.
//!
//! Gemini ends a malformed function call, a prompt block, and a candidate
//! filter this way. The text before the stop is not an answer: a compaction
//! summary built from it would replace the history with half a document.
//!
//! An integration test because a keyed client builds a credential pool, and
//! the pool persists under `BORG_HOME`. Each test pins `BORG_HOME` to a fresh
//! tempdir behind one lock, so nothing touches the developer's `~/.borg`.

#![allow(clippy::await_holding_lock, clippy::expect_used)]

use std::sync::Mutex;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use cortex::config::Config;
use cortex::llm::LlmClient;
use cortex::security::secrets_resolve::SecretRef;
use cortex::types::Message;

/// Serializes the tests in this binary: each one sets `BORG_HOME`.
static SERIAL: Mutex<()> = Mutex::new(());

async fn gemini_client(server: &MockServer) -> LlmClient {
    let mut config = Config::default();
    config.llm.provider = Some("gemini".to_string());
    config.llm.model = "gemini-2.5-flash".to_string();
    config.llm.api_key = Some(SecretRef::Literal {
        value: "test-key".to_string(),
    });
    config.llm.base_url = Some(server.uri());
    config.llm.max_retries = 0;
    LlmClient::new(&config).await.expect("client builds")
}

#[tokio::test]
async fn a_stream_the_provider_blocks_is_not_an_answer() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("tempdir");
    // SAFETY: `SERIAL` keeps every env write in this binary on one thread.
    unsafe {
        std::env::set_var("BORG_HOME", home.path());
    }

    let partial =
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"The user asked \"}]}}]}\n\n";
    let cases = [
        // Failed part-way: some text, then an error event, then a normal close.
        (
            "malformed function call",
            format!("{partial}data: {{\"candidates\":[{{\"finishReason\":\"MALFORMED_FUNCTION_CALL\"}}]}}\n\n"),
            "MALFORMED_FUNCTION_CALL",
        ),
        // Filtered part-way: some text, then a filter stop, then a normal close.
        (
            "candidate SAFETY",
            format!("{partial}data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"for\"}}]}},\"finishReason\":\"SAFETY\"}}]}}\n\n"),
            "SAFETY",
        ),
        (
            "candidate RECITATION",
            format!("{partial}data: {{\"candidates\":[{{\"finishReason\":\"RECITATION\"}}]}}\n\n"),
            "RECITATION",
        ),
        // Blocked before any text: the error event is the whole stream.
        (
            "prompt block",
            "data: {\"promptFeedback\":{\"blockReason\":\"PROHIBITED_CONTENT\"}}\n\n".to_string(),
            "PROHIBITED_CONTENT",
        ),
    ];

    for (name, sse, reason) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        let mut client = gemini_client(&server).await;

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.chat_streamed_checked(&[Message::user("summarize")]),
        )
        .await
        .expect("stream must complete promptly");

        let err = outcome.expect_err(name);
        assert!(
            err.to_string().contains(reason),
            "{name}: the error must name the block: {err}"
        );
    }
}
