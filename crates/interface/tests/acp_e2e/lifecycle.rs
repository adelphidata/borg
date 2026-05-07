//! Daemon lifecycle: concurrent sessions, cancellation, daemon crash mid-
//! turn, malformed-then-recover.
//!
//! Each test exercises a different way the connection between `borg acp` and
//! `borgd` can be perturbed and pins what the IDE-facing JSON-RPC must look
//! like in the perturbed state.

#![allow(dead_code)]

use super::harness::{initialize_params_with_fs, simple_text_turn, AcpClient, BorgdFixture};
use borgd::session::scripted::{ScriptedBackend, ScriptedFactory};
use cortex::agent::AgentEvent;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test]
async fn two_concurrent_sessions_have_independent_event_streams() {
    // Real failure mode: a regression that broadcast the FIRST session's
    // events to all subscribers would have the IDE see "session A's tool
    // call" appear on session B's chat. Tests that the daemon's
    // per-session buffer keeps streams isolated.
    let backend_a = ScriptedBackend::fixed("session-A", simple_text_turn("from-A"));
    let backend_b = ScriptedBackend::fixed("session-B", simple_text_turn("from-B"));
    // ScriptedFactory::queue pops FIFO; the first session/new pulls the
    // *last* pushed. We push B first so that A is popped first.
    let factory = ScriptedFactory::queue(vec![backend_b, backend_a]);
    let fixture = BorgdFixture::with_factory(factory).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");

    let cwd = fixture.workspace_path().display().to_string();
    let new_a = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new A");
    let sid_a = new_a["result"]["sessionId"].as_str().unwrap().to_string();
    let new_b = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new B");
    let sid_b = new_b["result"]["sessionId"].as_str().unwrap().to_string();
    assert_ne!(sid_a, sid_b, "concurrent sessions must have distinct ids");

    let outcome_a = client.run_prompt(&sid_a, "go").await.expect("prompt A");
    let outcome_b = client.run_prompt(&sid_b, "go").await.expect("prompt B");

    assert_eq!(outcome_a.agent_message_text(), "from-A");
    assert_eq!(outcome_b.agent_message_text(), "from-B");
    assert_eq!(outcome_a.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(outcome_b.stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn cancel_mid_turn_terminates_with_cancelled_stop_reason() {
    // Real failure mode: a regression that ignored ACP `session/cancel`
    // would let the IDE's stop button do nothing — the agent keeps
    // streaming until it naturally completes. The script parks
    // until cancel fires, then emits Error("cancelled"); the ACP
    // transport must translate that to stop_reason=cancelled.
    let backend = ScriptedBackend::new("cancel-test", move |_t, _i, tx, cancel, _b| {
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::TextDelta("starting...".into())).await;
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(AgentEvent::Error("cancelled".into())).await;
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Watchdog: if cancel never fires (test broken), still
                    // terminate so the test fails loudly rather than hangs.
                    let _ = tx.send(AgentEvent::TurnComplete).await;
                }
            }
        })
    });
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    // Send the cancel notification 100ms after we kick off the prompt so
    // the daemon definitely sees the StartTurn frame first. We can do
    // this concurrently because `notify` takes `&self` while
    // `run_prompt` takes `&mut self` — but tokio futures running on the
    // same task can interleave at await points without aliasing.
    //
    // We use `tokio::join!` with one future capturing `&mut client`
    // (run_prompt) and a side-channel cancel via a cloned stdin sender.
    // The cleanest expression: split the client into prompt + cancel
    // halves by storing the cancel-side via `inject_raw_bytes` against
    // a pre-built JSON frame. That keeps the borrow checker happy.
    let cancel_frame = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": sid},
    }))
    .expect("serialize cancel");
    let mut cancel_bytes = cancel_frame;
    cancel_bytes.push(b'\n');

    // Snapshot the inject handle BEFORE the borrow on `client` for
    // run_prompt. `inject_raw_bytes` only needs `&self`, but Rust's
    // borrow checker treats the whole struct as borrowed during
    // run_prompt; pre-extracting the bytes plus a reference to a thin
    // helper lets the cancel land via a fresh task.
    let injector = ClientInjector::from(&client);
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        injector.inject(cancel_bytes).await
    });

    let outcome = client.run_prompt(&sid, "park").await.expect("prompt");
    cancel_task
        .await
        .expect("cancel task join")
        .expect("inject");

    assert_eq!(
        outcome.stop_reason.as_deref(),
        Some("cancelled"),
        "session/cancel must translate to stop_reason=cancelled; got {outcome:?}"
    );
    assert!(
        outcome.agent_message_text().contains("starting..."),
        "deltas emitted before cancel must reach the IDE: {outcome:?}"
    );
}

/// Tiny stdin-only handle to an [`AcpClient`]: holds a clone of the same
/// [`tokio::sync::mpsc::Sender`] the client uses to feed bytes to the
/// subprocess's stdin. Lets a concurrent task send a cancel frame without
/// needing `&mut` on the whole client.
struct ClientInjector {
    stdin_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl ClientInjector {
    fn from(client: &AcpClient) -> Self {
        Self {
            stdin_tx: client.stdin_sender(),
        }
    }
    async fn inject(self, bytes: Vec<u8>) -> Result<(), anyhow::Error> {
        self.stdin_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow::anyhow!("subprocess stdin closed"))
    }
}

#[tokio::test]
async fn cancel_after_completion_is_a_silent_noop_response() {
    // Real failure mode: a regression that errored on cancel-after-end
    // would surface a spurious error in the IDE every time the user
    // hit "stop" on an already-complete turn. Plan §"Cancellation"
    // says cancel after completion is a no-op.
    let backend = ScriptedBackend::fixed("cancel-after", simple_text_turn("done"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    let outcome = client.run_prompt(&sid, "go").await.expect("prompt");
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    // Now cancel a turn that's already done. ACP `session/cancel` is a
    // *notification* (no id, no response expected); a request shape
    // would also be fine — neither must error.
    client
        .notify("session/cancel", json!({"sessionId": sid}))
        .await
        .expect("cancel notification");

    // The subprocess must remain alive and continue serving. Issue
    // another initialize-shaped request as a liveness probe.
    let _ = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new after cancel-noop must still succeed");
}

#[tokio::test]
async fn malformed_json_then_valid_request_recovers_in_subprocess() {
    // Real failure mode: a regression that poisoned the inbound parser
    // after a malformed line would brick the subprocess for the rest
    // of the IDE's session. Plan §"Logging and Stdio Safety" requires
    // the parser to recover line-by-line.
    let backend = ScriptedBackend::fixed("recover", simple_text_turn("alive"));
    let fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");

    client
        .inject_raw_bytes(b"this is not json\n")
        .await
        .expect("inject malformed");

    // Valid request after the garbage line MUST still work.
    let init = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    assert!(init.get("result").is_some(), "subprocess didn't recover");

    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new after garbage");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    let outcome = client.run_prompt(&sid, "go").await.expect("prompt");
    assert_eq!(outcome.agent_message_text(), "alive");
}

#[tokio::test]
async fn daemon_crash_mid_turn_surfaces_error_to_ide() {
    // Real failure mode: if `borgd` crashes during a turn and the
    // subprocess silently waits forever, the IDE spinner spins until the
    // user kills it. Plan §"Daemon failures" requires: surface an error
    // to the in-flight request and exit non-zero so the IDE re-spawns.
    let proceed_cancel = Arc::new(Notify::new());
    let backend = ScriptedBackend::new("crash-test", move |_t, _i, tx, _c, _b| {
        let proceed = proceed_cancel.clone();
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::TextDelta("about-to-die".into())).await;
            // Park forever — the daemon will be killed before TurnComplete.
            let _ = proceed.notified().await;
            let _ = tx.send(AgentEvent::TurnComplete).await;
        })
    });
    let mut fixture = BorgdFixture::with_backend(backend).await;
    let mut client = AcpClient::spawn(&fixture).await.expect("spawn");
    let _ = client
        .request("initialize", initialize_params_with_fs())
        .await
        .expect("initialize");
    let cwd = fixture.workspace_path().display().to_string();
    let new = client
        .request("session/new", json!({"cwd": cwd}))
        .await
        .expect("session/new");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    // Spawn the prompt, then crash the daemon while it's in flight.
    let prompt_task = {
        let sid = sid.clone();
        tokio::spawn(async move { client.run_prompt(&sid, "go").await })
    };

    // Let the prompt actually reach the daemon before tearing it down.
    tokio::time::sleep(Duration::from_millis(150)).await;
    fixture.shutdown_now().await;

    // The in-flight prompt must resolve — either as an error envelope or
    // as a transport-level failure that surfaces through `run_prompt`'s
    // result. We do NOT require a specific stop_reason; what matters is
    // the IDE doesn't hang.
    let outcome_or_err = tokio::time::timeout(Duration::from_secs(10), prompt_task)
        .await
        .expect("prompt task did not return after daemon crash")
        .expect("prompt task panicked");

    match outcome_or_err {
        Ok(outcome) => {
            assert!(
                outcome.error.is_some() || outcome.stop_reason.as_deref() == Some("cancelled"),
                "daemon crash must surface as error or cancelled stop reason; got {outcome:?}"
            );
        }
        Err(_) => {
            // run_prompt itself returned Err (timeout/transport closed).
            // That's also acceptable — the failure was visible to the
            // caller; the IDE would not hang.
        }
    }
}

// Sanity helper: shape-check that ParamsValue is a JSON-RPC error envelope.
fn assert_is_error_envelope(envelope: &Value, code: i64) {
    let err = envelope
        .get("error")
        .unwrap_or_else(|| panic!("expected error envelope; got {envelope}"));
    assert_eq!(err["code"], json!(code));
}
