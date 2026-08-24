//! Inbound-to-outbound pipeline tests over a script-backed channel.
//!
//! These live in their own test binary because they install the
//! process-global `SessionExecutor` (a `OnceLock`), which would leak into
//! unit tests that assert the no-executor behavior.
//!
//! Hermetic: `BORG_HOME` points at a tempdir, the agent is a stub that
//! echoes the turn text, and channel scripts are local bash scripts in a
//! tempdir. No real network, no real `~/.borg`, no channel credentials.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::db::Database;
use gateway::channel::registry::RegisteredChannel;
use gateway::handler::{handle_polled_message, invoke_agent, InboundMessage};
use gateway::session_executor::{InboundTurn, SessionExecutor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Serializes tests that repoint `BORG_HOME` — env vars are process-global.
fn borg_home_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct EchoExec;

#[async_trait::async_trait]
impl SessionExecutor for EchoExec {
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _ = tx
                .send(AgentEvent::TextDelta(format!("echo:{}", turn.text)))
                .await;
            let _ = tx.send(AgentEvent::TurnComplete).await;
        });
        Ok((rx, CancellationToken::new()))
    }
    async fn cancel(&self, _session_key: &str) -> bool {
        false
    }
    async fn cancel_all(&self) -> usize {
        0
    }
    async fn active_turns(&self) -> usize {
        0
    }
}

/// Install the echo stub as the process-global executor exactly once.
fn install_echo_executor() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| gateway::session_executor::set_global(Arc::new(EchoExec)));
}

fn script_channel(dir: &std::path::Path, manifest_toml: &str) -> RegisteredChannel {
    let manifest: gateway::manifest::ChannelManifest = toml::from_str(manifest_toml).unwrap();
    RegisteredChannel {
        manifest,
        dir: dir.to_path_buf(),
    }
}

fn write_script(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), format!("#!/bin/bash\n{body}\n")).unwrap();
}

fn open_dm_config() -> Config {
    let mut config = Config::default();
    config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;
    config
}

fn inbound_with_text(sender_id: &str, text: &str) -> InboundMessage {
    serde_json::from_value(serde_json::json!({
        "sender_id": sender_id,
        "text": text,
    }))
    .unwrap()
}

/// (payload_json, status, error) rows for a channel, in insert order.
async fn delivery_rows(
    db: &Database,
    channel: &str,
) -> Vec<(serde_json::Value, String, Option<String>)> {
    let stmt = db
        .conn()
        .prepare(
            "SELECT payload_json, status, error FROM delivery_queue \
             WHERE channel_name = ?1 ORDER BY rowid ASC",
        )
        .await
        .unwrap();
    let mut rows_iter = stmt
        .query(libsql::params![channel.to_string()])
        .await
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = rows_iter.next().await.unwrap() {
        rows.push((
            serde_json::from_str(&row.get::<String>(0).unwrap()).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<Option<String>>(2).unwrap(),
        ));
    }
    rows
}

#[tokio::test]
async fn polled_message_round_trips_agent_reply_to_outbound_delivery() {
    let _guard = borg_home_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("BORG_HOME", tmp.path());
    install_echo_executor();

    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "inbound.sh", "echo '{}'");
    write_script(dir.path(), "outbound.sh", r#"echo '{"ok": true}'"#);
    let channel = script_channel(
        dir.path(),
        r#"
name = "roundtrip-chan"
description = "test"
runtime = "bash"

[scripts]
inbound = "inbound.sh"
outbound = "outbound.sh"
"#,
    );

    let msg = inbound_with_text("dm-user", "PIPELINE_MARKER_ALPHA");
    let out = handle_polled_message(&channel, msg, &open_dm_config(), None)
        .await
        .unwrap();

    // The stub agent's reply (echo of the prepared turn text) must come
    // back to the caller — not a canned or empty string.
    assert!(
        out.contains("echo:") && out.contains("PIPELINE_MARKER_ALPHA"),
        "got: {out}"
    );

    // One delivery row: delivered, unchunked, and free of credentials.
    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .unwrap();
    let rows = delivery_rows(&db, "roundtrip-chan").await;
    std::env::remove_var("BORG_HOME");
    assert_eq!(rows.len(), 1, "rows: {rows:?}");
    let (payload, status, _err) = &rows[0];
    assert_eq!(status, "delivered");
    assert!(payload.get("chunk_index").is_none(), "payload: {payload}");
    assert!(payload.get("total_chunks").is_none(), "payload: {payload}");
    assert!(payload.get("token").is_none(), "payload: {payload}");
    assert!(
        payload["text"]
            .as_str()
            .unwrap()
            .contains("PIPELINE_MARKER_ALPHA"),
        "payload: {payload}"
    );
}

#[tokio::test]
async fn long_reply_is_chunked_with_chunk_metadata() {
    let _guard = borg_home_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("BORG_HOME", tmp.path());
    install_echo_executor();

    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "inbound.sh", "echo '{}'");
    write_script(dir.path(), "outbound.sh", r#"echo '{"ok": true}'"#);
    let channel = script_channel(
        dir.path(),
        r#"
name = "chunky-chan"
description = "test"
runtime = "bash"

[scripts]
inbound = "inbound.sh"
outbound = "outbound.sh"

[settings]
max_message_chars = 40
"#,
    );

    // The echoed reply is well past 40 chars, so it must be split and
    // every chunk annotated with its position.
    let msg = inbound_with_text("dm-user-chunks", &"B".repeat(200));
    let out = handle_polled_message(&channel, msg, &open_dm_config(), None)
        .await
        .unwrap();
    assert!(out.len() > 40, "reply long enough to need chunking: {out}");

    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .unwrap();
    let rows = delivery_rows(&db, "chunky-chan").await;
    std::env::remove_var("BORG_HOME");
    assert!(rows.len() >= 2, "expected multiple chunks, rows: {rows:?}");
    for (i, (payload, status, _)) in rows.iter().enumerate() {
        assert_eq!(status, "delivered");
        assert_eq!(
            payload["chunk_index"].as_u64(),
            Some(i as u64),
            "payload: {payload}"
        );
        assert_eq!(
            payload["total_chunks"].as_u64(),
            Some(rows.len() as u64),
            "payload: {payload}"
        );
    }
}

#[tokio::test]
async fn failed_outbound_send_is_recorded_on_the_delivery_row() {
    let _guard = borg_home_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("BORG_HOME", tmp.path());
    install_echo_executor();

    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "inbound.sh", "echo '{}'");
    // Exit 4 is the permanent-failure code: no retries, fail immediately.
    write_script(dir.path(), "outbound.sh", "echo doomed >&2\nexit 4");
    let channel = script_channel(
        dir.path(),
        r#"
name = "failing-chan"
description = "test"
runtime = "bash"

[scripts]
inbound = "inbound.sh"
outbound = "outbound.sh"

[settings]
retry_max_attempts = 1
retry_initial_delay_ms = 1
"#,
    );

    let msg = inbound_with_text("dm-user-fail", "FAIL_MARKER");
    handle_polled_message(&channel, msg, &open_dm_config(), None)
        .await
        .unwrap();

    // The delivery row must reflect the failure — exhausted, with the
    // script error preserved for `borg doctor`/replay tooling.
    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .unwrap();
    let rows = delivery_rows(&db, "failing-chan").await;
    std::env::remove_var("BORG_HOME");
    assert_eq!(rows.len(), 1, "rows: {rows:?}");
    let (_payload, status, error) = &rows[0];
    assert_eq!(status, "exhausted", "rows: {rows:?}");
    assert!(
        error.as_deref().unwrap_or("").contains("exit 4"),
        "rows: {rows:?}"
    );
}

#[tokio::test]
async fn invoke_agent_returns_agent_reply_and_real_session_id() {
    let _guard = borg_home_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("BORG_HOME", tmp.path());
    install_echo_executor();

    let msg = inbound_with_text("direct-caller", "INVOKE_MARKER_BETA");
    let (resp, session_id) = invoke_agent("unittest-chan", &msg, &open_dm_config(), None, None)
        .await
        .unwrap();
    std::env::remove_var("BORG_HOME");

    assert!(
        resp.contains("echo:") && resp.contains("INVOKE_MARKER_BETA"),
        "got: {resp}"
    );
    assert!(!session_id.is_empty(), "session id must be the real session");
}
