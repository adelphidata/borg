//! Live-MCP test lane: talks to a real MCP server (today: `uvx
//! mcp-server-time`) over the real stdio transport. Gated by both the
//! `live-mcp` cargo feature AND `BORG_ALLOW_LIVE_TESTS=1` so default
//! `cargo test` stays hermetic. Mirrors `cortex`'s `live-llm` lane.
//!
//! Run with:
//!
//! ```sh
//! BORG_ALLOW_LIVE_TESTS=1 cargo test -p mcp --features live-mcp \
//!     --test live_mcp
//! ```
//!
//! Requires `uvx` (from `uv` / `pipx`) and network access for the first
//! invocation of `uvx mcp-server-time` (subsequent runs use the cached
//! tool). If `uvx` is not on `$PATH`, the test logs and exits cleanly so
//! the live lane stays opt-in without a hard infra dependency.

#![cfg(feature = "live-mcp")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::time::Duration;

use mcp::manifest::{ServerManifest, ToolFilter, Transport as TransportSpec};
use mcp::session::SessionActor;
use mcp::transport::{http::HttpPolicy, open};
use serde_json::json;

fn live_enabled() -> bool {
    matches!(
        std::env::var("BORG_ALLOW_LIVE_TESTS").as_deref(),
        Ok("1" | "true" | "TRUE" | "True")
    )
}

fn have_binary(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn time_manifest() -> ServerManifest {
    ServerManifest {
        name: "time".into(),
        transport: TransportSpec::Stdio {
            command: "uvx".into(),
            args: vec!["mcp-server-time".into()],
            env: HashMap::new(),
        },
        connect_timeout: Duration::from_secs(30),
        call_timeout: Duration::from_secs(15),
        rate_limit_per_minute: 60,
        rate_limit_burst: 10,
        tools: ToolFilter {
            include: None,
            exclude: vec![],
            read_only: vec!["get_current_time".into(), "convert_time".into()],
            max: ToolFilter::DEFAULT_MAX,
        },
    }
}

/// Real-stdio E2E: spawn `uvx mcp-server-time`, complete the MCP
/// handshake, list tools, dispatch `get_current_time`, and assert the
/// reply is a valid RFC3339 timestamp.
///
/// Failure modes proven by this test:
///   - stdio framing regression (e.g. accidentally writing
///     Content-Length when the server uses LSP-newline)
///   - handshake protocol-version drift
///   - tool-call payload mismatch with a real server's schema
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mcp_server_time_stdio_round_trip() {
    if !live_enabled() {
        eprintln!(
            "skipping live_mcp_server_time_stdio_round_trip: set \
             BORG_ALLOW_LIVE_TESTS=1 to run the live MCP lane"
        );
        return;
    }
    if !have_binary("uvx") {
        eprintln!(
            "skipping live_mcp_server_time_stdio_round_trip: `uvx` not on PATH — \
             install `uv` to run this lane",
        );
        return;
    }

    let manifest = time_manifest();
    let transport = open(&manifest, HttpPolicy::default())
        .await
        .expect("open uvx mcp-server-time");
    let mut session = SessionActor::new(&manifest, transport);

    tokio::time::timeout(manifest.connect_timeout, session.handshake())
        .await
        .expect("handshake within connect_timeout")
        .expect("handshake ok");

    let discovered = session.list_tools().await.expect("tools/list");
    // mcp-server-time historically advertises {get_current_time,
    // convert_time}. Two is the minimum for this test to be meaningful;
    // future versions may add more tools, which is fine.
    assert!(
        discovered.tools.len() >= 2,
        "mcp-server-time should expose at least 2 tools; got {:?}",
        discovered.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    let names: Vec<&str> = discovered.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"get_current_time"),
        "expected `get_current_time` in tools list, got {names:?}",
    );

    // Dispatch — `get_current_time` requires a `timezone` argument (IANA
    // tz). UTC is universally available.
    let (text, _raw) = session
        .call_tool("get_current_time", json!({"timezone": "UTC"}))
        .await
        .expect("tools/call get_current_time");

    // The server returns a JSON envelope:
    //   {"timezone":"UTC","datetime":"2026-…","is_dst":false}
    // Parse the JSON and run `datetime` through chrono so a server-side
    // schema change (or a corrupted timestamp) fails the test for the
    // right reason. A loose substring sniff would tautologically pass
    // for the envelope itself.
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("tool result must be JSON; got {text:?} (err {e})"));
    let datetime_str = parsed
        .get("datetime")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("tool result missing `datetime` string field: {parsed}"));
    chrono::DateTime::parse_from_rfc3339(datetime_str)
        .unwrap_or_else(|e| panic!("`datetime` field is not RFC3339: {datetime_str:?} (err {e})"));

    // Second call to exercise multi-request stdio framing. A regression
    // that lost the per-request id correlation would surface here as a
    // hang or a mismatched response.
    let (text2, _raw) = session
        .call_tool(
            "convert_time",
            json!({
                "source_timezone": "UTC",
                "time": "12:00",
                "target_timezone": "America/Los_Angeles",
            }),
        )
        .await
        .expect("tools/call convert_time");
    assert!(
        !text2.is_empty(),
        "convert_time returned empty payload — stdio framing regression?",
    );
}
