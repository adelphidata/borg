//! Integration tests for the URL policy gate as it wires into
//! `web_fetch` (HTTP via wiremock) and the `browser` tool handler.
//!
//! These tests are deliberately end-to-end at the tool-handler layer:
//! they exercise the production code path the LLM actually drives, not
//! the pure `classify` function (which is covered by unit tests).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cortex::config::{Config, ThrottleRule, TransformAction, TransformRule};
use cortex::tool_handlers::handle_web_fetch;
use cortex::types::ToolOutput;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

fn base_config() -> Config {
    let mut config = Config::default();
    config.web.enabled = true;
    // Hosts under test are local (127.0.0.1) — without this they'd be
    // blocked by the SSRF private-IP gate.
    config.security.url_policy.allow_private_urls = true;
    config
}

// ────────────────────────────────────────────────────────────────────
// web_fetch — pre-flight block
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_fetch_blocks_cloud_metadata_without_network_call() {
    // Stand up a server but never expect it to be hit.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
        .expect(0)
        .mount(&server)
        .await;

    let config = base_config();
    // Cloud-metadata IP is hard-blocked regardless of allow_private_urls.
    let result = handle_web_fetch(
        &json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
        &config,
    )
    .await
    .unwrap();
    assert!(
        result.contains("URL policy blocked") && result.contains("ssrf_blocked_ip"),
        "expected URL policy block, got: {result}"
    );
}

#[tokio::test]
async fn web_fetch_blocks_denylisted_host_without_network_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = base_config();
    let uri = server.uri();
    // Pull the host:port out and put it on the denylist.
    let parsed = url::Url::parse(&uri).unwrap();
    let host = parsed.host_str().unwrap().to_string();
    config.security.url_policy.denylist = vec![host.clone()];

    let result = handle_web_fetch(&json!({ "url": uri }), &config)
        .await
        .unwrap();
    assert!(
        result.contains("URL policy blocked"),
        "expected URL policy block, got: {result}"
    );
    assert!(
        result.contains(&host),
        "block message should name the host, got: {result}"
    );
}

// ────────────────────────────────────────────────────────────────────
// web_fetch — redirect re-validation
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_fetch_blocks_redirect_to_metadata_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("Location", "http://169.254.169.254/latest/"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let config = base_config();
    let start = format!("{}/start", server.uri());
    let result = handle_web_fetch(&json!({ "url": start }), &config)
        .await
        .unwrap();
    // The error surface depends on reqwest's redirect-error translation;
    // we just need to see that the request did NOT succeed and the cause
    // is our policy callback, not a network failure.
    assert!(
        result.to_lowercase().contains("redirect") || result.contains("URL policy"),
        "expected redirect block, got: {result}"
    );
}

#[tokio::test]
async fn web_fetch_blocks_redirect_to_denylisted_host() {
    // A real-world misuse case: a benign-looking URL 301-redirects to a
    // tracker / paywall mirror the operator has denylisted. The redirect
    // callback must refuse to follow even though the original URL was
    // fine. This is distinct from the metadata-IP case (which is
    // hard-blocked SSRF) — denylist matches are user-defined and apply
    // only at the rule layer.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("Location", "http://tracker.example.test/x"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = base_config();
    config.security.url_policy.denylist = vec!["tracker.example.test".to_string()];
    let start = format!("{}/start", server.uri());
    let result = handle_web_fetch(&json!({ "url": start }), &config)
        .await
        .unwrap();
    assert!(
        (result.to_lowercase().contains("redirect") || result.contains("URL policy"))
            && result.contains("tracker.example.test"),
        "expected redirect-to-denylist block naming the host, got: {result}"
    );
}

// ────────────────────────────────────────────────────────────────────
// web_fetch — transform: utm_source stripped before request hits server
// ────────────────────────────────────────────────────────────────────

struct CountingResponder(Arc<AtomicUsize>);

impl Respond for CountingResponder {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        self.0.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/plain")
            .set_body_string("ok")
    }
}

#[tokio::test]
async fn web_fetch_applies_strip_query_params_transform() {
    let server = MockServer::start().await;
    let hits = Arc::new(AtomicUsize::new(0));

    // The mock only matches when `q=1` is present AND `utm_source` is absent;
    // wiremock has no "not contains" matcher, but we can prove it positively:
    // the request URL the server records includes the query string. We
    // assert the server saw exactly one matching request and zero others.
    Mock::given(method("GET"))
        .and(path("/x"))
        .and(query_param("q", "1"))
        .respond_with(CountingResponder(hits.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let mut config = base_config();
    // Transform applies to the wiremock host.
    let parsed = url::Url::parse(&server.uri()).unwrap();
    let host = parsed.host_str().unwrap().to_string();
    config.security.url_policy.transform_rules = vec![TransformRule {
        pattern: host,
        action: TransformAction::StripQueryParams {
            names: vec!["utm_source".into(), "utm_medium".into()],
        },
    }];

    let url = format!(
        "{}/x?utm_source=newsletter&q=1&utm_medium=email",
        server.uri()
    );
    let result = handle_web_fetch(&json!({ "url": url }), &config)
        .await
        .unwrap();
    assert_eq!(result.trim(), "ok", "expected fetched body, got: {result}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Verify the recorded request had only the kept param.
    let received = server.received_requests().await.unwrap();
    let req = received
        .iter()
        .find(|r| r.url.path() == "/x")
        .expect("request to /x");
    let kept: Vec<(String, String)> = req
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(kept, vec![("q".to_string(), "1".to_string())]);
}

// ────────────────────────────────────────────────────────────────────
// Throttle surfaces via the handler
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_fetch_handler_surfaces_throttle_to_caller() {
    // Each test gets a wiremock server on a fresh port, so the throttle
    // map (keyed by host:port) is naturally clean for this test's host.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("first"),
        )
        // We only expect ONE network call. The second should be throttled
        // pre-flight.
        .expect(1)
        .mount(&server)
        .await;

    let mut config = base_config();
    let parsed = url::Url::parse(&server.uri()).unwrap();
    let host = parsed.host_str().unwrap().to_string();
    config.security.url_policy.throttle_rules = vec![ThrottleRule {
        pattern: host,
        min_interval_ms: 60_000, // ensure the second call is in-window
    }];

    let url = server.uri();
    let first = handle_web_fetch(&json!({ "url": url.clone() }), &config)
        .await
        .unwrap();
    assert!(first.contains("first"), "first call body: {first}");

    let second = handle_web_fetch(&json!({ "url": url }), &config)
        .await
        .unwrap();
    assert!(
        second.contains("throttled") && second.contains("retry in"),
        "expected throttle message, got: {second}"
    );
}

// ────────────────────────────────────────────────────────────────────
// Browser handler — Block path returns before launching Chrome
// ────────────────────────────────────────────────────────────────────
//
// We can't construct a BrowserSession in tests (no headless Chrome
// guaranteed in CI), so we drive the public `handle_browser` entrypoint
// and assert the URL policy fires BEFORE the lazy-launch step. Success
// criterion: the handler returns an "Error: URL policy blocked …" string
// AND `session` remains `None` (no launch attempted).

#[tokio::test]
async fn browser_navigate_blocked_url_does_not_launch_chrome() {
    use cortex::browser::BrowserSession;
    use cortex::tool_handlers::handle_browser;

    let mut config = Config::default();
    config.browser.enabled = true;
    config.security.url_policy.denylist = vec!["evil.example".into()];

    let mut session: Option<BrowserSession> = None;
    let out = handle_browser(
        &json!({
            "action": "navigate",
            "url": "https://evil.example/path",
        }),
        &config,
        &mut session,
    )
    .await
    .unwrap();

    let text = match out {
        ToolOutput::Text(s) => s,
        _ => panic!("expected text output"),
    };
    assert!(
        text.contains("URL policy blocked") && text.contains("evil.example"),
        "expected policy block message, got: {text}"
    );
    assert!(
        session.is_none(),
        "browser should not have been launched on a blocked URL"
    );
}

#[tokio::test]
async fn browser_navigate_metadata_ip_blocked_pre_launch() {
    use cortex::browser::BrowserSession;
    use cortex::tool_handlers::handle_browser;

    let mut config = Config::default();
    config.browser.enabled = true;
    // Even with private-URL toggle on, metadata IPs stay blocked.
    config.security.url_policy.allow_private_urls = true;

    let mut session: Option<BrowserSession> = None;
    let out = handle_browser(
        &json!({
            "action": "navigate",
            "url": "http://169.254.169.254/",
        }),
        &config,
        &mut session,
    )
    .await
    .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        _ => panic!("expected text output"),
    };
    assert!(
        text.contains("URL policy blocked") && text.contains("ssrf_blocked_ip"),
        "expected SSRF block, got: {text}"
    );
    assert!(session.is_none(), "no browser launch on SSRF block");
}
