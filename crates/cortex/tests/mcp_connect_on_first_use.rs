//! What `reload_from_config` owes a server whose start has not landed yet.
//!
//! A cached catalog advertises a server's tools before the real connection
//! exists, so there is a window where the server is neither absent nor live.
//! The start registry is what makes that window safe; these are its two
//! observable contracts. The registry's own state machine — claiming, waiting,
//! and the message each terminal state produces — is unit-tested in
//! `mcp_clients::tests::start_registry`.
//!
//! **A removed server's start is retired with it.** The teardown loop bumps the
//! epoch *and* cancels the start, so a call parked on it wakes at once instead
//! of sitting out a connect timeout for a server that no longer exists.
//!
//! **One child per server.** The reload computed its "already running" set from
//! the session map alone, which has no entry for a server still starting — so
//! it queued that server for a *second* spawn. The proof is a stdio command
//! that appends a line every time it is executed: the file must hold exactly
//! one line however the reload and the start interleave.
//!
//! Both use stdio servers running `sleep` — a child that spawns but never
//! speaks MCP — so the start stays in flight for as long as the test needs.

#![allow(clippy::expect_used, clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cortex::config::{Config, McpServerConfig, TransportSpec};
use cortex::mcp_clients;
use mcp::manifest::ToolFilter;

/// Serialize: these mutate the process-wide `BORG_HOME` and share the
/// global MCP registry and start map. Tokio's mutex is held across awaits and
/// does not poison, so one test's panic cannot cascade.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pin `BORG_HOME` to a fresh tempdir so the catalog-cache probe opens a
/// scratch DB, never the operator's live `~/.borg`.
fn isolated_data_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: `SERIAL` guarantees only one test in this binary mutates env at a
    // time; no other thread reads `BORG_HOME` concurrently.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
    }
    tmp
}

fn stdio_server(command: &str, connect_timeout_ms: u64) -> McpServerConfig {
    McpServerConfig {
        enabled: true,
        transport: TransportSpec::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), command.into()],
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
async fn removing_a_server_retires_its_in_flight_start_immediately() {
    let _guard = SERIAL.lock().await;
    let _data_dir = isolated_data_dir();
    mcp_clients::testing::reset_all();

    // Long enough that "the slot went away" cannot be the start finishing on
    // its own — the child sleeps 30s and the connect timeout is 20s.
    const CONNECT_MS: u64 = 20_000;
    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    cfg.mcp.startup_grace_ms = 1;
    cfg.mcp
        .servers
        .insert("doomed".into(), stdio_server("sleep 30", CONNECT_MS));

    mcp_clients::initialize(&cfg).await.expect("initialize");
    assert!(
        mcp_clients::testing::is_start_in_flight("doomed"),
        "the silent server's start should still be running"
    );

    // The operator removes it. A call parked on this start must be released
    // now, not left to sit out a 20s connect timeout for a server that no
    // longer exists — which is what happens if the teardown loop bumps the
    // epoch without also cancelling the start.
    let mut removed_cfg = cfg.clone();
    removed_cfg.mcp.servers.clear();
    let started = Instant::now();
    let report = mcp_clients::reload_from_config(&removed_cfg)
        .await
        .expect("reload_from_config");
    let elapsed = started.elapsed();

    assert_eq!(report.removed, vec!["doomed".to_string()], "{report:?}");
    assert!(
        !mcp_clients::testing::is_start_in_flight("doomed"),
        "the start slot must be retired with the server"
    );
    assert!(
        elapsed < Duration::from_millis(CONNECT_MS / 4),
        "teardown waited on the start ({elapsed:?}); it must cancel, not block"
    );

    mcp_clients::testing::reset_all();
}

#[tokio::test]
async fn a_reload_racing_an_in_flight_start_spawns_exactly_one_child() {
    let _guard = SERIAL.lock().await;
    let data_dir = isolated_data_dir();
    mcp_clients::testing::reset_all();

    // Every execution of this command appends one line. The line count is the
    // number of children spawned for this server — the thing item 6 is about.
    let spawn_log = data_dir.path().join("spawns.txt");
    let command = format!("echo x >> {}; sleep 30", spawn_log.display());

    const CONNECT_MS: u64 = 5_000;
    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    cfg.mcp.startup_grace_ms = 1;
    cfg.mcp
        .servers
        .insert("racer".into(), stdio_server(&command, CONNECT_MS));

    mcp_clients::initialize(&cfg).await.expect("initialize");
    assert!(
        mcp_clients::testing::is_start_in_flight("racer"),
        "the start must still be in flight for this race to mean anything"
    );

    // The reload sees the same desired server. Before the fix it found no
    // session entry, put `racer` in `to_add`, and spawned a second child.
    let report = mcp_clients::reload_from_config(&cfg)
        .await
        .expect("reload_from_config");
    assert!(
        report.added.is_empty(),
        "an in-flight server must not be reported as newly added: {:?}",
        report.added
    );
    assert_eq!(report.unchanged, 1, "it belongs in unchanged: {report:?}");

    // Give the child time to have written, so a second spawn would show up.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let spawns = std::fs::read_to_string(&spawn_log).unwrap_or_default();
    let count = spawns.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        count, 1,
        "expected exactly one child for 'racer', got {count} (log: {spawns:?})"
    );

    mcp_clients::testing::reset_all();
}
