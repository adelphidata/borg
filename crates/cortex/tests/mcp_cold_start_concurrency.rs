//! Cold-start latency contract for `mcp_clients::initialize`.
//!
//! On a catalog-cache miss, servers used to start sequentially inline: one
//! unreachable server's connect timeout queued in front of every other
//! server and the daemon's boot. The contract now is (a) cache-miss servers
//! start concurrently, and (b) `mcp.startup_grace_ms` bounds how long
//! `initialize` waits at all — stragglers keep starting in the background.
//!
//! Both tests use stdio servers running `sleep` — a child that spawns but
//! never speaks MCP — so the handshake hits its full connect timeout, which
//! is exactly the pathological case the concurrency exists for. Timing
//! bounds are asserted with wide margins (the sequential path would take at
//! least the *sum* of the timeouts; we assert well under it).

#![allow(clippy::expect_used, clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cortex::config::{Config, McpServerConfig, TransportSpec};
use cortex::mcp_clients;
use mcp::manifest::ToolFilter;

/// Serialize tests in this binary: both mutate the process-wide
/// `BORG_DATA_DIR` env var and share the global MCP registry. Tokio's mutex
/// is safe to hold across the `initialize` await and doesn't poison, so one
/// test's panic can't cascade into the other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pin `BORG_DATA_DIR` to a fresh tempdir so the catalog-cache probe inside
/// `initialize` opens a scratch DB, never the operator's live `~/.borg`.
fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: `SERIAL` guarantees only one test in this binary mutates env
    // at a time; no other thread reads `BORG_DATA_DIR` concurrently.
    unsafe {
        std::env::set_var("BORG_DATA_DIR", tmp.path());
    }
    tmp
}

/// A stdio server whose child spawns fine but never completes the MCP
/// handshake, so startup burns the full connect timeout.
fn silent_server(connect_timeout_ms: u64) -> McpServerConfig {
    McpServerConfig {
        enabled: true,
        transport: TransportSpec::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: HashMap::new(),
        },
        connect_timeout_ms,
        call_timeout_ms: connect_timeout_ms,
        rate_limit_per_minute: 60,
        rate_limit_burst: 10,
        tools: ToolFilter::default(),
        cache_tools: false,
    }
}

#[tokio::test]
async fn cache_miss_servers_start_concurrently_not_sequentially() {
    let _guard = SERIAL.lock().await;
    let _data_dir = isolated_data_dir();

    const CONNECT_MS: u64 = 1_500;
    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    // Grace far above both timeouts: initialize waits for every server to
    // resolve, so elapsed time measures the starts themselves.
    cfg.mcp.startup_grace_ms = 30_000;
    cfg.mcp
        .servers
        .insert("conc_silent_a".into(), silent_server(CONNECT_MS));
    cfg.mcp
        .servers
        .insert("conc_silent_b".into(), silent_server(CONNECT_MS));

    let started = Instant::now();
    mcp_clients::initialize(&cfg)
        .await
        .expect("initialize tolerates failing servers");
    let elapsed = started.elapsed();

    // Sequential starts would take >= 2 * CONNECT_MS. Concurrent starts
    // finish in ~CONNECT_MS; the margin below absorbs slow CI.
    assert!(
        elapsed < Duration::from_millis(2 * CONNECT_MS - 100),
        "two failing servers took {elapsed:?} — at least the sum of their \
         connect timeouts, i.e. they started sequentially",
    );
    assert!(
        elapsed >= Duration::from_millis(CONNECT_MS - 100),
        "sanity: with grace above the timeout, initialize should have \
         waited out at least one connect timeout, took {elapsed:?}",
    );
}

#[tokio::test]
async fn startup_grace_bounds_initialize_and_leaves_stragglers_in_background() {
    let _guard = SERIAL.lock().await;
    let _data_dir = isolated_data_dir();

    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    // The server needs 20 s to fail; the boot budget is 200 ms. initialize
    // must return on the grace, not on the server.
    cfg.mcp.startup_grace_ms = 200;
    cfg.mcp
        .servers
        .insert("grace_straggler".into(), silent_server(20_000));

    let started = Instant::now();
    mcp_clients::initialize(&cfg)
        .await
        .expect("initialize returns on grace expiry");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "initialize took {elapsed:?} — it blocked on a straggler's connect \
         timeout instead of returning when the startup grace expired",
    );
    // The straggler never completed its handshake, so it must not have
    // registered any tools at the moment initialize returned.
    assert!(
        !mcp_clients::tool_definitions()
            .iter()
            .any(|t| t.function.name.contains("grace_straggler")),
        "a still-starting server must not advertise tools yet",
    );
}
