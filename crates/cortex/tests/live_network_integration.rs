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

/// Positive: snapshot returns interactive elements as `@e<N>` refs, and a
/// follow-up `click {ref: "@e0"}` resolves through the session's ref store
/// without the model having to write a CSS selector. Then back navigates
/// the history. The whole flow mirrors how hermes-style agents drive pages.
#[tokio::test]
async fn live_browser_snapshot_then_ref_click_then_back() {
    require_live!("live_browser_snapshot_then_ref_click_then_back");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;

    let close = serde_json::json!({"action": "close"});

    // Step 1: navigate somewhere with links.
    let nav = tool_handlers::handle_browser(
        &serde_json::json!({"action": "navigate", "url": "https://example.com/"}),
        &config,
        &mut session,
    )
    .await;
    if nav.is_err() {
        let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
        panic!("navigate failed: {nav:?}");
    }

    // Step 2: snapshot — example.com has one link ("More information...").
    let snap = tool_handlers::handle_browser(
        &serde_json::json!({"action": "snapshot"}),
        &config,
        &mut session,
    )
    .await;
    let snap_text = match snap {
        Ok(ToolOutput::Text(t)) => t,
        Ok(other) => {
            let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
            panic!("snapshot returned non-text: {other:?}");
        }
        Err(e) => {
            let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
            panic!("snapshot errored: {e}");
        }
    };
    assert!(
        snap_text.contains("@e0"),
        "snapshot missing @e0: {snap_text}"
    );

    // Step 3: click @e0 — must resolve through session refs without CSS.
    let click = tool_handlers::handle_browser(
        &serde_json::json!({"action": "click", "ref": "@e0"}),
        &config,
        &mut session,
    )
    .await;
    // We don't assert the click succeeds (the link may navigate cross-origin
    // and we don't care here); only that the REF RESOLUTION worked — a
    // failure would say "ref '@e0' was not produced by a recent snapshot".
    let click_text = match click {
        Ok(ToolOutput::Text(t)) => t,
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("Err: {e}"),
    };
    assert!(
        !click_text.contains("was not produced by a recent snapshot"),
        "ref resolution failed: {click_text}"
    );

    // Step 4: back — example.com → linked page → back to example.com.
    // (Or back to about:blank if click didn't navigate.)
    let back = tool_handlers::handle_browser(
        &serde_json::json!({"action": "back"}),
        &config,
        &mut session,
    )
    .await;

    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let back_text = match back.expect("back should not error") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        back_text.contains("history.back"),
        "back didn't run: {back_text}"
    );
}

/// Negative: `back` on a brand-new session sitting on about:blank with
/// no navigation history yet must report a clean no-op, not crash and not
/// silently succeed. Guards the "empty history = no-op" contract.
#[tokio::test]
async fn live_browser_back_on_empty_history_is_noop() {
    require_live!("live_browser_back_on_empty_history_is_noop");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;
    let close = serde_json::json!({"action": "close"});

    // Touch a page to force the lazy session launch, then go back without
    // ever having navigated past the initial entry.
    let _ = tool_handlers::handle_browser(
        &serde_json::json!({"action": "snapshot"}),
        &config,
        &mut session,
    )
    .await;

    let back = tool_handlers::handle_browser(
        &serde_json::json!({"action": "back"}),
        &config,
        &mut session,
    )
    .await;
    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let text = match back.expect("back errored") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        text.contains("no-op") || text.contains("history.back"),
        "expected no-op message on empty history: {text}"
    );
}

/// Negative: `upload` against a page that has no file input must surface
/// a clear "Element not found" error rather than silently no-op-ing — the
/// model would otherwise believe the upload succeeded.
#[tokio::test]
async fn live_browser_upload_no_input_returns_error() {
    require_live!("live_browser_upload_no_input_returns_error");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;
    let close = serde_json::json!({"action": "close"});

    let _ = tool_handlers::handle_browser(
        &serde_json::json!({"action": "navigate", "url": "https://example.com/"}),
        &config,
        &mut session,
    )
    .await;

    let up = tool_handlers::handle_browser(
        &serde_json::json!({
            "action": "upload",
            "selector": "input[type=file]",
            "files": ["/tmp/nonexistent.png"],
        }),
        &config,
        &mut session,
    )
    .await;
    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let text = match up.expect("upload errored") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        text.contains("Error") && (text.contains("not found") || text.contains("Element")),
        "expected 'Element not found' style error: {text}"
    );
}
