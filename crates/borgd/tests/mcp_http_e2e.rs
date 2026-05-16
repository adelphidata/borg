//! End-to-end smoke for the MCP HTTP surface mounted by
//! `gateway::GatewayServer`.
//!
//! Real failure mode guarded:
//! - A regression that drops `.merge(build_mcp_http_router(...))` from
//!   `crates/gateway/src/server.rs` — exactly the bug shipped to main
//!   before this test existed. Asserted via "POST /mcp returns 200 with
//!   a valid `tools/list` JSON-RPC reply, not 404."
//! - A regression that exposes a mutating tool over MCP — asserted by
//!   intersecting `tools/list` with `McpServer::MUTATING_TOOLS`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

#[path = "ingress_e2e_common.rs"]
mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_http_tools_list_round_trip() {
    let harness = common::boot_gateway(false).await;
    let token = common::issue_token("e2e-mcp").await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    // 1. Missing bearer → 401 (route mounted + auth gate intact).
    let no_auth = client
        .post(harness.url("/mcp"))
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        }))
        .send()
        .await
        .expect("send no-auth");
    assert_eq!(no_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 2. tools/list returns the v1 read-only set.
    let resp = client
        .post(harness.url("/mcp"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        }))
        .send()
        .await
        .expect("tools/list");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    let tools = body["result"]["tools"]
        .as_array()
        .expect("result.tools is array");
    let names: Vec<String> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "memory_search".to_string(),
            "read_memory".into(),
            "web_fetch".into(),
            "web_search".into(),
            "list".into(),
        ]
    );

    // 3. None of the documented mutating tools leaked in.
    for forbidden in gateway::mcp::McpServer::MUTATING_TOOLS {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "mutating tool `{forbidden}` must never appear over MCP"
        );
    }

    // 4. initialize handshake works (real MCP clients send this first).
    let resp = client
        .post(harness.url("/mcp"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"initialize","params":{}
        }))
        .send()
        .await
        .expect("initialize");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["result"]["serverInfo"]["name"], "borg-mcp");
    assert!(
        body["result"]["protocolVersion"]
            .as_str()
            .unwrap_or("")
            .starts_with("2024-"),
        "protocolVersion should be a dated MCP spec; got: {}",
        body["result"]["protocolVersion"]
    );

    harness.shutdown().await;
}
