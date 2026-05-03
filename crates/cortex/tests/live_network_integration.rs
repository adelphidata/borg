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
use cortex::tool_handlers;
use cortex::types::ToolOutput;

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
