//! Live-LLM test lane: exercises `web_fetch`, `web_search`, and `browser`
//! against real network targets.
//!
//! Run with:
//!
//! ```sh
//! BORG_ALLOW_LIVE_TESTS=1 cargo test -p cortex --features live-llm \
//!     --test live_network_integration
//! ```
//!
//! Builds without `BORG_ALLOW_LIVE_TESTS=1` skip the network bodies so a
//! feature-only build still passes offline.

#![cfg(feature = "live-llm")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use cortex::browser::BrowserSession;
use cortex::config::Config;
use cortex::llm::LlmClient;
use cortex::tool_handlers;
use cortex::types::{ContentPart, MediaData, Message, ToolOutput};

fn live_enabled() -> bool {
    match std::env::var("BORG_ALLOW_LIVE_TESTS") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

macro_rules! require_live {
    ($name:expr) => {
        if !live_enabled() {
            eprintln!(
                "skipping {}: set BORG_ALLOW_LIVE_TESTS=1 to run network tests",
                $name
            );
            return;
        }
    };
}

#[tokio::test]
async fn live_web_fetch_example_dot_com() {
    require_live!("live_web_fetch_example_dot_com");

    let config = Config::default();
    let args = serde_json::json!({"url": "https://example.com/"});
    let body = tool_handlers::handle_web_fetch(&args, &config)
        .await
        .expect("handle_web_fetch should not return Err");

    assert!(
        body.contains("Example Domain"),
        "expected example.com anchor text in body, got: {body}"
    );
}

#[tokio::test]
async fn live_web_search_returns_results() {
    require_live!("live_web_search_returns_results");

    let config = Config::default();
    let args = serde_json::json!({"query": "rust programming language", "mode": "live"});
    let body = tool_handlers::handle_web_search(&args, &config)
        .await
        .expect("handle_web_search should not return Err");

    // The provider's response is provider-specific JSON/text; assert only
    // that it is non-empty and not the disabled / error shape.
    assert!(
        !body.is_empty(),
        "web_search returned empty body for 'rust programming language'"
    );
    assert!(
        !body.contains("disabled"),
        "web_search reported disabled despite default config: {body}"
    );
    assert!(
        !body.starts_with("Error searching:"),
        "web_search returned an error: {body}"
    );
}

#[tokio::test]
async fn live_browser_navigate_example_dot_com() {
    require_live!("live_browser_navigate_example_dot_com");

    let mut config = Config::default();
    // The default browser config gates on `enabled`; tests opt in.
    config.browser.enabled = true;

    let mut session: Option<BrowserSession> = None;
    let args = serde_json::json!({
        "action": "navigate",
        "url": "https://example.com/",
    });
    // Run navigate, then ALWAYS close before any assertion that could panic —
    // otherwise a failed navigate would leak the Chrome process.
    let nav_result = tool_handlers::handle_browser(&args, &config, &mut session).await;
    let close_args = serde_json::json!({"action": "close"});
    let _ = tool_handlers::handle_browser(&close_args, &config, &mut session).await;

    let out = nav_result.expect("handle_browser should not return Err");
    let text = match &out {
        ToolOutput::Text(t) => t.clone(),
        ToolOutput::Multimodal { text, .. } => text.clone(),
    };

    assert!(
        !text.starts_with("Error"),
        "browser navigate failed (is Chrome installed?): {text}"
    );
}

// ── Gemini native transport live lane ────────────────────────────────────
//
// Gates the M2/M3/M4 native Gemini adapter behind the same `live-llm` +
// `BORG_ALLOW_LIVE_TESTS=1` opt-in as the rest of this file. The default
// hermetic `cargo test` lane never reaches `:generateContent`. Requires
// `GEMINI_API_KEY` to be set in addition to the opt-in env var — without
// the key the test skips with a clear message rather than failing.

fn gemini_key_present() -> Option<String> {
    std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

fn gemini_client(model: &str) -> LlmClient {
    let mut config = Config::default();
    config.llm.provider = Some("gemini".to_string());
    config.llm.model = model.to_string();
    // LlmClient::new reads the API key from this env var name.
    config.llm.api_key_env = "GEMINI_API_KEY".to_string();
    LlmClient::new(&config).expect("LlmClient construction for Gemini")
}

#[tokio::test]
async fn live_gemini_native_text_chat_returns_non_empty_reply() {
    require_live!("live_gemini_native_text_chat_returns_non_empty_reply");
    let Some(_key) = gemini_key_present() else {
        eprintln!("skipping live_gemini_native_text_chat: GEMINI_API_KEY not set");
        return;
    };

    let client = gemini_client("gemini-2.5-flash");
    let messages = vec![Message::user("Reply with exactly one word: ping.")];

    let reply = client
        .chat(&messages, None)
        .await
        .expect("Gemini chat must succeed against the live API");

    let body = reply
        .content
        .as_ref()
        .map(|c| c.full_text())
        .unwrap_or_default();
    assert!(
        !body.trim().is_empty(),
        "Gemini chat returned empty text body: {reply:?}"
    );
}

#[tokio::test]
async fn live_gemini_native_video_inline_data_returns_description() {
    require_live!("live_gemini_native_video_inline_data_returns_description");
    let Some(_key) = gemini_key_present() else {
        eprintln!(
            "skipping live_gemini_native_video_inline_data_returns_description: GEMINI_API_KEY not set"
        );
        return;
    };

    // Smallest mp4 fixture we can ship without a binary asset: a 4-byte
    // size header + ftyp box + brand. The model will reject malformed
    // payloads with a clear error rather than silently succeeding, so even
    // this synthetic input is a valid wire-format smoke test.
    let mp4_bytes: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2', 0x00, 0x00, 0x00,
        0x00, b'i', b's', b'o', b'm', b'm', b'p', b'4', b'2',
    ];
    let engine = base64::engine::general_purpose::STANDARD;
    let b64 = <base64::engine::GeneralPurpose as base64::Engine>::encode(&engine, &mp4_bytes);

    let client = gemini_client("gemini-2.5-pro");
    let messages = vec![Message::user_multimodal(vec![
        ContentPart::Text("Describe this video in one sentence.".into()),
        ContentPart::VideoBase64 {
            media: MediaData {
                mime_type: "video/mp4".into(),
                data: b64,
                filename: Some("tiny.mp4".into()),
            },
        },
    ])];

    // The model may legitimately refuse a non-decodable stream — what we're
    // really asserting here is that the request shape reaches Gemini
    // (i.e. `inline_data` survived JSON serialisation), the auth succeeds,
    // and we get *some* assistant reply back rather than a transport error.
    let reply = client
        .chat(&messages, None)
        .await
        .expect("Gemini video chat must succeed (request shape + auth)");
    let body = reply
        .content
        .as_ref()
        .map(|c| c.full_text())
        .unwrap_or_default();
    assert!(
        !body.trim().is_empty(),
        "Gemini returned empty reply for video request: {reply:?}"
    );
}
