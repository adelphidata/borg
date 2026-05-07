//! ACP `fs/read_text_file` + `fs/write_text_file` bridge.
//!
//! These tests drive the round-trip through the daemon's `AcpTurn` bidi
//! stream into the subprocess's [`JsonRpcCallbackClient`], which writes a
//! `fs/*` request to stdout, the harness's [`crate::harness::FsMap`] answers
//! it, and the bridge wakes the script. Production tool handlers (read_file,
//! apply_patch, run_shell) call the bridge through
//! `cortex::execution_context::TextFileBridge`; we exercise the same trait
//! method via `bridge.read_text_file(...)` from a [`ScriptedBackend`] script
//! so the assertions stay focused on the wire/IO contract rather than on
//! whatever a real LLM might do with the result.
//!
//! The bridge implementation in `borgd::grpc::session::GrpcAcpTextFileBridge`
//! uses `tokio::task::block_in_place`, so every test in this file MUST run
//! under the multi-thread runtime — single-thread current_thread panics at
//! `block_in_place`.

#![allow(dead_code)]

use super::harness::{initialize_params_no_fs, initialize_params_with_fs, AcpClient, BorgdFixture};
use borgd::session::scripted::ScriptedBackend;
use cortex::agent::AgentEvent;
use cortex::execution_context::TextFileBridge;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// Boot the standard fixture/client/initialize/session-new sequence with the
/// given `initialize` parameters and a script captured by reference. The
/// script is `Arc<...>` so test bodies can keep a handle to inspect what the
/// bridge round-trip produced.
async fn boot_with_script<F>(initialize: Value, script: F) -> (BorgdFixture, AcpClient, String)
where
    F: Send
        + 'static
        + FnMut(
            String,
            Vec<borgd::session::backend::InlineImage>,
            tokio::sync::mpsc::Sender<AgentEvent>,
            tokio_util::sync::CancellationToken,
            Option<Arc<dyn TextFileBridge>>,
        ) -> tokio::task::JoinHandle<()>,
{
    let backend = ScriptedBackend::new("fs-bridge", script);
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn borg acp");
    let _ = client
        .request("initialize", initialize)
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    (fixture, client, sid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_text_file_roundtrips_through_acp_callback() {
    // Real failure mode: a regression that wired the bridge to disk would
    // bypass the IDE's unsaved buffer and read the on-disk version. The
    // unsaved-buffer access promise is a core ACP value-prop — pin it by
    // arranging FsMap to return content that does NOT match disk.
    let (fixture, mut client, session_id) = boot_with_script(
        initialize_params_with_fs(),
        move |_t, _i, tx, _c, bridge| {
            tokio::spawn(async move {
                let bridge = bridge.expect("bridge must be present when fs caps are advertised");
                let content = bridge
                    .read_text_file(Path::new("foo.txt"))
                    .expect("bridge.read_text_file");
                let _ = tx
                    .send(AgentEvent::TextDelta(format!("READ:{content}")))
                    .await;
                let _ = tx.send(AgentEvent::TurnComplete).await;
            })
        },
    )
    .await;

    // Disk has nothing; FsMap returns the unsaved-buffer content.
    let on_disk_path = fixture.workspace_path().join("foo.txt");
    std::fs::write(&on_disk_path, "stale-on-disk").expect("seed disk");
    client.fs_map().insert("foo.txt", "live-from-buffer");

    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    let text = outcome.agent_message_text();
    assert!(
        text.contains("READ:live-from-buffer"),
        "bridge must surface FsMap content, not on-disk content; got {text:?}"
    );
    assert!(
        client
            .fs_map()
            .observed_reads()
            .iter()
            .any(|p| p == "foo.txt"),
        "FsMap.observed_reads must record the bridge call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_text_file_surfaces_client_error_without_falling_back_to_disk() {
    // Real failure mode: silently falling back to disk when the IDE's read
    // fails (file not opened, permissions, etc.) would mask the IDE's
    // intent and leak unsaved-buffer-only content. The bridge MUST surface
    // the error to the script.
    let (fixture, mut client, session_id) = boot_with_script(
        initialize_params_with_fs(),
        move |_t, _i, tx, _c, bridge| {
            tokio::spawn(async move {
                let bridge = bridge.expect("bridge present");
                let result = bridge.read_text_file(Path::new("denied.txt"));
                match result {
                    Ok(content) => {
                        let _ = tx
                            .send(AgentEvent::TextDelta(format!("UNEXPECTED_OK:{content}")))
                            .await;
                    }
                    Err(e) => {
                        let _ = tx.send(AgentEvent::TextDelta(format!("ERR:{e}"))).await;
                    }
                }
                let _ = tx.send(AgentEvent::TurnComplete).await;
            })
        },
    )
    .await;

    // Seed disk so a silent fallback would observably succeed.
    std::fs::write(
        fixture.workspace_path().join("denied.txt"),
        "should-not-be-read",
    )
    .expect("seed disk");
    client.fs_map().fail_read("denied.txt", "permission denied");

    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    let text = outcome.agent_message_text();
    assert!(
        text.starts_with("ERR:"),
        "bridge must propagate the FsMap error; got: {text:?}"
    );
    assert!(
        text.contains("permission denied"),
        "the error message must round-trip from the harness; got: {text:?}"
    );
    assert!(
        !text.contains("should-not-be-read"),
        "the bridge MUST NOT fall back to disk on client error; got: {text:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_text_file_roundtrips_through_acp_callback() {
    // Real failure mode: a regression that bypassed the bridge for writes
    // would persist directly to disk and the IDE would see the file change
    // out from under it. The harness FsMap records every write — pin that
    // a write through the bridge lands there, not on disk.
    let (fixture, mut client, session_id) = boot_with_script(
        initialize_params_with_fs(),
        move |_t, _i, tx, _c, bridge| {
            tokio::spawn(async move {
                let bridge = bridge.expect("bridge present");
                bridge
                    .write_text_file(Path::new("notes.txt"), "hello-from-bridge")
                    .expect("bridge.write_text_file");
                let _ = tx.send(AgentEvent::TextDelta("WROTE".into())).await;
                let _ = tx.send(AgentEvent::TurnComplete).await;
            })
        },
    )
    .await;

    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    let writes = client.fs_map().observed_writes();
    assert!(
        writes
            .iter()
            .any(|(p, c)| p == "notes.txt" && c == "hello-from-bridge"),
        "FsMap.observed_writes must record bridge.write_text_file; got {writes:?}"
    );
    // The bridge MUST NOT touch disk; the on-disk file should not exist.
    assert!(
        !fixture.workspace_path().join("notes.txt").exists(),
        "bridge must not write to disk when ACP fs caps are on"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_fs_capabilities_means_no_bridge_attached() {
    // Real failure mode: a regression that attached a bridge regardless of
    // capabilities would let an IDE that explicitly opted out (no `fs.*`
    // declared) still receive `fs/read_text_file` requests, which it
    // wouldn't know how to answer. The plan §"Capability advertisement"
    // says: bridge present iff at least one fs cap is on.
    let (_fix, mut client, session_id) =
        boot_with_script(initialize_params_no_fs(), move |_t, _i, tx, _c, bridge| {
            tokio::spawn(async move {
                let marker = if bridge.is_some() {
                    "BRIDGE_ATTACHED"
                } else {
                    "NO_BRIDGE"
                };
                let _ = tx.send(AgentEvent::TextDelta(marker.into())).await;
                let _ = tx.send(AgentEvent::TurnComplete).await;
            })
        })
        .await;

    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    assert_eq!(outcome.agent_message_text(), "NO_BRIDGE");
    assert!(
        client.fs_map().observed_reads().is_empty(),
        "no fs requests should reach the harness when caps are absent"
    );
    assert!(client.fs_map().observed_writes().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_file_write_with_partial_failure_can_drive_rollback() {
    // Real failure mode: a multi-file edit (think apply_patch) that fails
    // halfway must be able to restore the prior content via the same bridge
    // — disk-mode fallback would defeat the purpose of routing writes
    // through the IDE. Production callers (apply_patch) implement
    // best-effort rollback by re-issuing writes with the original content.
    // We don't test apply_patch here (cortex covers that); we pin the
    // bridge primitives the rollback depends on.
    let fs_map_initial = vec![
        ("a.txt".to_string(), "ORIG_A".to_string()),
        ("b.txt".to_string(), "ORIG_B".to_string()),
        ("c.txt".to_string(), "ORIG_C".to_string()),
    ];
    let (_fix, mut client, session_id) = boot_with_script(
        initialize_params_with_fs(),
        move |_t, _i, tx, _c, bridge| {
            tokio::spawn(async move {
                let bridge = bridge.expect("bridge present");
                // Attempt writes a, b, c. `c` will fail; rollback reverts a + b.
                let _ = bridge.write_text_file(Path::new("a.txt"), "NEW_A");
                let _ = bridge.write_text_file(Path::new("b.txt"), "NEW_B");
                let third = bridge.write_text_file(Path::new("c.txt"), "NEW_C");
                if third.is_err() {
                    let _ = bridge.write_text_file(Path::new("a.txt"), "ORIG_A");
                    let _ = bridge.write_text_file(Path::new("b.txt"), "ORIG_B");
                    let _ = tx.send(AgentEvent::TextDelta("ROLLED_BACK".into())).await;
                } else {
                    let _ = tx.send(AgentEvent::TextDelta("WROTE_ALL".into())).await;
                }
                let _ = tx.send(AgentEvent::TurnComplete).await;
            })
        },
    )
    .await;

    for (p, c) in &fs_map_initial {
        client.fs_map().insert(p, c);
    }
    client.fs_map().fail_write("c.txt", "disk full");

    let outcome = client
        .run_prompt(&session_id, "go")
        .await
        .expect("session/prompt");
    assert_eq!(outcome.agent_message_text(), "ROLLED_BACK");

    let writes = client.fs_map().observed_writes();
    let trail: Vec<(String, String)> = writes.into_iter().collect();
    assert_eq!(
        trail,
        vec![
            ("a.txt".into(), "NEW_A".into()),
            ("b.txt".into(), "NEW_B".into()),
            ("c.txt".into(), "NEW_C".into()),
            ("a.txt".into(), "ORIG_A".into()),
            ("b.txt".into(), "ORIG_B".into()),
        ],
        "bridge must record every write attempt in order; rollback restores via the SAME bridge"
    );
}
