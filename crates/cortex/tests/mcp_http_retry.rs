//! End-to-end test for streamable-HTTP MCP startup retry.
//!
//! A transient `5xx`/`429` during `initialize` used to permanently fail the
//! server until a daemon restart. The startup path now retries by recreating
//! the transport, bounded by `mcp.startup_max_retries`. This drives the real
//! `mcp_clients::initialize` path against a wiremock server that always 503s and
//! asserts the loop made exactly `1 + startup_max_retries` attempts (each a
//! fresh transport → fresh POST), then gave up cleanly without registering the
//! server. The happy "retry-then-succeed" handshake is covered by the
//! `ScriptedTransport` cases in `mcp_integration.rs`; here we pin the retry
//! count and transport recreation, which a static fixture can't fake.

#![allow(clippy::expect_used, clippy::field_reassign_with_default)]

use std::collections::HashMap;

use cortex::config::{Config, HttpAuthSpec, McpServerConfig, TransportSpec};
use cortex::mcp_clients;
use mcp::manifest::ToolFilter;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn http_startup_retries_transient_503_up_to_the_configured_bound() {
    let server = MockServer::start().await;
    // Every POST is a transient 503 — the retry loop should keep recreating the
    // transport and re-`initialize`-ing until the bound is hit.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    // wiremock binds loopback + plaintext; allow both so the URL policy admits it.
    cfg.mcp.allow_plaintext_http = true;
    cfg.mcp.allow_local_network = true;
    cfg.mcp.startup_max_retries = 2; // → 3 attempts (1 + 2 retries)
    cfg.mcp.startup_retry_base_ms = 1; // keep the backoff negligible for the test

    let sc = McpServerConfig {
        enabled: true,
        transport: TransportSpec::Http {
            url: format!("{}/mcp", server.uri()),
            headers: HashMap::new(),
            auth: HttpAuthSpec::None,
            client_cert: None,
            client_key: None,
            ca_cert: None,
        },
        connect_timeout_ms: 2_000,
        call_timeout_ms: 2_000,
        rate_limit_per_minute: 60,
        rate_limit_burst: 10,
        tools: ToolFilter::default(),
        cache_tools: true,
    };
    cfg.mcp.servers.insert("flaky".into(), sc);

    // initialize() logs + continues per-server, so a failing server is Ok(()).
    mcp_clients::initialize(&cfg)
        .await
        .expect("initialize tolerates a failing server");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock records requests");
    assert_eq!(
        requests.len(),
        3,
        "startup should attempt initialize exactly 1 + startup_max_retries times \
         (each a recreated transport), got {} POSTs",
        requests.len()
    );

    // A server that never completed startup must register no tools.
    assert!(
        !mcp_clients::tool_definitions()
            .iter()
            .any(|t| t.function.name.contains("flaky")),
        "a server that failed every startup attempt must not register tools"
    );
}
