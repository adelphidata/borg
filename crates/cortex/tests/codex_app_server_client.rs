//! Stage 1 of the codex app-server runtime: end-to-end test of the JSON-RPC
//! client against a hermetic fake `codex` binary (`src/bin/fake_codex_app_server.rs`).
//!
//! These tests exercise the actual subprocess + stdio path — not a mock of the
//! client's internals — so a regression in framing, request/response correlation,
//! or notification ordering will fail here.
//!
//! Gated on the `test-support` feature so the fixture binary is not built into
//! release artifacts. Run with: `cargo test -p cortex --features test-support`.

#![cfg(feature = "test-support")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use cortex::codex_app_server::{ClientError, CodexAppServerClient, RequestId, SpawnOptions};

fn fake_codex_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_codex_app_server"))
}

fn spawn_options() -> SpawnOptions {
    SpawnOptions {
        codex_bin: fake_codex_path(),
        ..Default::default()
    }
}

#[tokio::test]
async fn full_handshake_and_turn_drains_notifications() {
    let client = CodexAppServerClient::spawn(spawn_options())
        .await
        .expect("spawn fake codex");

    let init = client
        .initialize("borg-test", "Borg Test", "0.0")
        .await
        .expect("initialize");
    assert_eq!(init["userAgent"], "fake-codex/0.0");
    assert_eq!(init["platformOs"], "test");

    // thread/start
    let thread = client
        .request(
            "thread/start",
            serde_json::json!({}),
            Duration::from_secs(2),
        )
        .await
        .expect("thread/start");
    assert_eq!(thread["threadId"], "thread-fake-1");

    // turn/start: the response shouldn't arrive until after the streaming
    // notifications, so a successful request implies the reader didn't deadlock
    // on a backed-up notification queue.
    let turn = client
        .request(
            "turn/start",
            serde_json::json!({ "prompt": "hi" }),
            Duration::from_secs(5),
        )
        .await
        .expect("turn/start");
    assert_eq!(turn["promptId"], "p1");

    // We expect exactly 4 notifications (3 item/* + 1 turn/completed), in order.
    let mut methods = Vec::new();
    for _ in 0..4 {
        let notif = tokio::time::timeout(Duration::from_secs(2), client.take_notification())
            .await
            .expect("timed out waiting for notification")
            .expect("notification channel closed");
        methods.push(notif.method);
    }
    assert_eq!(
        methods,
        vec![
            "item/agentMessage",
            "item/commandExecution",
            "item/agentMessage",
            "turn/completed",
        ]
    );

    client.close(Duration::from_secs(2)).await.expect("close");
}

#[tokio::test]
async fn unknown_method_surfaces_as_rpc_error() {
    let client = CodexAppServerClient::spawn(spawn_options())
        .await
        .expect("spawn");
    let _ = client
        .initialize("borg-test", "Borg Test", "0.0")
        .await
        .expect("init");

    let err = client
        .request(
            "method/does-not-exist",
            serde_json::json!({}),
            Duration::from_secs(2),
        )
        .await
        .expect_err("method should error");
    match err {
        ClientError::Rpc { code, message, .. } => {
            assert_eq!(code, -32601, "JSON-RPC method-not-found code");
            assert!(message.contains("method not found"));
        }
        other => panic!("expected ClientError::Rpc, got {other:?}"),
    }
    client.close(Duration::from_secs(2)).await.expect("close");
}

#[tokio::test]
async fn server_initiated_request_lands_on_queue() {
    let client = CodexAppServerClient::spawn(spawn_options())
        .await
        .expect("spawn");
    let _ = client
        .initialize("borg-test", "Borg Test", "0.0")
        .await
        .expect("init");

    let result = client
        .request(
            "request/approval-probe",
            serde_json::json!({}),
            Duration::from_secs(2),
        )
        .await
        .expect("approval-probe");
    assert!(result.is_object());

    let server_req = tokio::time::timeout(Duration::from_secs(2), client.take_server_request())
        .await
        .expect("timed out")
        .expect("server-request channel closed");
    assert_eq!(server_req.method, "exec/approve");
    assert_eq!(server_req.id, RequestId::Num(9000));
    assert_eq!(server_req.params["cmd"], "rm -rf /");

    // Respond so the wire stays clean — the fake doesn't strictly require it.
    client
        .respond(&server_req.id, serde_json::json!({ "decision": "approve" }))
        .await
        .expect("respond");
    client.close(Duration::from_secs(2)).await.expect("close");
}

#[tokio::test]
async fn request_to_garbage_peer_times_out_cleanly() {
    // FAKE_CODEX_MODE=garbage makes the fake emit malformed lines instead of
    // valid JSON-RPC, while keeping stdin open (so the subprocess does NOT
    // exit). The client must warn-and-continue on the bad line and surface a
    // `Timeout` — specifically Timeout, not Disconnected — once the request
    // deadline expires. Accepting either error would mask the EOF-race fix
    // (#1 in the code-reviewer punch list); the fake is deliberately
    // engineered to stay alive so this test can fail cleanly if the reader
    // mishandles malformed input.
    let opts = SpawnOptions {
        codex_bin: fake_codex_path(),
        env: vec![("FAKE_CODEX_MODE".to_string(), "garbage".to_string())],
        ..Default::default()
    };
    let client = CodexAppServerClient::spawn(opts).await.expect("spawn");
    assert!(client.is_alive().await, "fake should still be running");

    let err = client
        .request(
            "initialize",
            serde_json::json!({ "clientInfo": {}, "capabilities": {} }),
            Duration::from_millis(500),
        )
        .await
        .expect_err("garbage peer must surface timeout");
    match err {
        ClientError::Timeout {
            method,
            timeout_secs,
        } => {
            assert_eq!(method, "initialize");
            assert!(
                (0.4..=1.0).contains(&timeout_secs),
                "timeout_secs ~0.5, got {timeout_secs}"
            );
        }
        other => panic!("expected ClientError::Timeout (fake is still alive); got {other:?}"),
    }
    // Subprocess must still be running — the bug we're guarding against is the
    // reader exiting on malformed input, which would manifest as either a
    // Disconnected error above or an `is_alive() == false` here.
    assert!(
        client.is_alive().await,
        "subprocess unexpectedly exited on malformed input"
    );
    client.close(Duration::from_secs(2)).await.expect("close");
}

#[tokio::test]
async fn request_after_subprocess_exit_returns_disconnected() {
    // Guards the EOF race fix in `client.rs`: once `read_stdout_loop` observes
    // stdout EOF it must (a) drain in-flight waiters AND (b) set `reader_done`
    // so that *fresh* requests inserted afterward post-check the flag and
    // self-remove from `pending` instead of hanging on their oneshot until the
    // timeout fires.
    //
    // We MUST trigger the reader exit without going through `client.close()` —
    // close() also sets the `closed` flag, and `write_line` bails on that flag
    // before the reader_done check is ever reached, so the original test
    // passed whether or not the reader_done machinery existed. Here we use
    // `FAKE_CODEX_MODE=exit_after_init` so the fake responds to `initialize`
    // and then exits, dropping its stdout, which drives the client's reader to
    // EOF while `closed` stays false.
    let opts = SpawnOptions {
        codex_bin: fake_codex_path(),
        env: vec![("FAKE_CODEX_MODE".to_string(), "exit_after_init".to_string())],
        ..Default::default()
    };
    let client = CodexAppServerClient::spawn(opts).await.expect("spawn");
    let _ = client
        .initialize("borg-test", "Borg Test", "0.0")
        .await
        .expect("init");

    // Poll for the subprocess to actually exit. The reader sets `reader_done`
    // shortly after stdout EOF, which follows process exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while client.is_alive().await {
        if std::time::Instant::now() > deadline {
            panic!("fake did not exit within 2s of initialize response");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Grace so the reader's EOF handler completes and flips reader_done.
    // 250ms gives loaded CI machines headroom between OS-level child reap
    // and the tokio task waking to handle stdout EOF.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Fresh request: must fast-fail via the reader_done short-circuit, not via
    // a timeout or via write_line's `closed` check (which is still false).
    let started = std::time::Instant::now();
    let err = client
        .request(
            "thread/start",
            serde_json::json!({}),
            Duration::from_secs(60), // very generous: we expect fast-fail, not timeout
        )
        .await
        .expect_err("post-EOF request must error");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "request hung for {elapsed:?}; should fast-fail via Disconnected"
    );
    match &err {
        ClientError::Disconnected(msg) => {
            assert!(
                msg.contains("reader task already exited"),
                "Disconnected via wrong path: {msg:?} — \
                 expected the reader_done short-circuit, not write_line failure"
            );
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }

    client.close(Duration::from_secs(2)).await.expect("close");
}
