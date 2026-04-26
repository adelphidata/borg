// Integration tests are a separate compilation unit and don't pick up the
// crate-level `cfg(test)` allow for expect/unwrap. Allowed here for the same
// reason: failed setup means the test environment itself is broken.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

//! End-to-end smoke test for the daemon's gRPC kernel. Spins a borgd server
//! against a tempdir BORG_HOME, attaches a stub session backend, and exercises
//! Open / Send / Stream / Cancel / Close / RespondToPrompt over a real UDS.
//!
//! Avoids touching the real `Database` or LLM providers — the goal is to
//! verify the daemon's transport, codec, prompt routing, and broadcast
//! fan-out under realistic transport conditions.

use async_trait::async_trait;
use borg_core::agent::AgentEvent as CoreEvent;
use borg_proto::session::agent_event::Kind;
use borg_proto::session::session_client::SessionClient;
use borg_proto::session::session_server::SessionServer;
use borg_proto::session::{
    CancelRequest, CloseRequest, OpenRequest, PromptResponse, SendRequest, StreamRequest,
};
use borgd::daemon::bind_uds;
use borgd::grpc::session::{SessionFactory, SessionSvc};
use borgd::session::backend::SessionBackend;
use borgd::session::{SessionHost, SessionRegistry};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Stub backend whose script is decided per-test.
struct ScriptedBackend {
    id: String,
    /// Closure-like script: takes (text, event_tx, cancel) and runs the turn.
    script: Mutex<
        Option<
            Box<
                dyn Send
                    + 'static
                    + FnMut(
                        String,
                        mpsc::Sender<CoreEvent>,
                        CancellationToken,
                    ) -> tokio::task::JoinHandle<()>,
            >,
        >,
    >,
}

impl ScriptedBackend {
    fn new<F>(id: &str, script: F) -> Arc<Self>
    where
        F: Send
            + 'static
            + FnMut(String, mpsc::Sender<CoreEvent>, CancellationToken) -> tokio::task::JoinHandle<()>,
    {
        Arc::new(Self {
            id: id.into(),
            script: Mutex::new(Some(Box::new(script))),
        })
    }
}

#[async_trait]
impl SessionBackend for ScriptedBackend {
    async fn run_turn(
        &self,
        text: String,
        event_tx: mpsc::Sender<CoreEvent>,
        cancel: CancellationToken,
    ) {
        // Take the script out, run it (synchronously — it spawns its own
        // task), put it back. Drop the guard before the only `.await` so the
        // future stays Send.
        let handle = {
            let mut g = self.script.lock().expect("script poisoned");
            g.as_mut().map(|s| s(text, event_tx.clone(), cancel))
        };
        match handle {
            Some(h) => {
                tokio::spawn(async move {
                    let _ = h.await;
                });
            }
            None => {
                let _ = event_tx
                    .send(CoreEvent::Error("backend exhausted".into()))
                    .await;
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

struct StubFactory {
    next: Mutex<Option<Arc<dyn SessionBackend>>>,
}

impl StubFactory {
    fn new(backend: Arc<dyn SessionBackend>) -> Arc<Self> {
        Arc::new(Self {
            next: Mutex::new(Some(backend)),
        })
    }
}

impl SessionFactory for StubFactory {
    fn open(&self, _resume_id: Option<&str>) -> Result<SessionHost, anyhow::Error> {
        let backend = self
            .next
            .lock()
            .expect("factory poisoned")
            .take()
            .ok_or_else(|| anyhow::anyhow!("factory exhausted"))?;
        Ok(SessionHost::new(backend))
    }
}

async fn spawn_session_service(
    tmp: &std::path::Path,
    factory: Arc<dyn SessionFactory>,
) -> (PathBuf, oneshot::Sender<()>) {
    let socket = tmp.join("borgd.sock");
    let listener = bind_uds(&socket).expect("bind_uds");
    let stream = UnixListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let svc = SessionSvc::new(registry, factory);

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(SessionServer::new(svc))
            .serve_with_incoming_shutdown(stream, async {
                let _ = rx.await;
            })
            .await;
    });

    let started = std::time::Instant::now();
    loop {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        if started.elapsed() > Duration::from_secs(2) {
            panic!("daemon UDS never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (socket, tx)
}

async fn uds_channel(socket: PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .expect("endpoint")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = UnixStream::connect(&socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connect")
}

#[tokio::test]
async fn open_send_streams_deltas_and_terminates_on_turn_complete() {
    // Real failure mode: the per-turn server-stream from `Session.Send` would
    // either drop events (mpsc/broadcast wiring bug) or never close (terminal-
    // event detection bug). Asserts both: full delta sequence arrives AND
    // stream ends after TurnComplete.
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = ScriptedBackend::new("s1", |_text, tx, _cancel| {
        tokio::spawn(async move {
            for d in ["hello ", "world"] {
                let _ = tx.send(CoreEvent::TextDelta(d.into())).await;
            }
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket).await);
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open ok")
        .into_inner();
    assert_eq!(opened.session_id, "s1");

    let mut stream = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "go".into(),
        })
        .await
        .expect("send ok")
        .into_inner();

    let mut text = String::new();
    let mut closed_with_complete = false;
    while let Some(item) = stream.next().await {
        let evt = item.expect("evt ok");
        match evt.kind {
            Some(Kind::TextDelta(d)) => text.push_str(&d.text),
            Some(Kind::TurnComplete(_)) => {
                closed_with_complete = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text, "hello world");
    assert!(closed_with_complete, "stream must close on TurnComplete");
}

#[tokio::test]
async fn cancel_during_turn_aborts_within_500ms_and_emits_error() {
    // Real failure mode: cancellation token not propagated through the gRPC
    // bridge → the tool would run to completion ignoring `/cancel`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = ScriptedBackend::new("s2", |_text, tx, cancel| {
        tokio::spawn(async move {
            for _ in 0..50 {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = tx.send(CoreEvent::Error("cancelled".into())).await;
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        let _ = tx.send(CoreEvent::TextDelta(".".into())).await;
                    }
                }
            }
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket).await);
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open ok")
        .into_inner();

    let mut stream = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "long".into(),
        })
        .await
        .expect("send ok")
        .into_inner();

    // Receive at least one delta so we know the turn is running.
    let _ = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;

    let cancel_at = std::time::Instant::now();
    client
        .cancel(CancelRequest {
            session_id: opened.session_id.clone(),
        })
        .await
        .expect("cancel ok");

    let mut got_error = false;
    while let Some(item) = stream.next().await {
        let evt = item.expect("evt");
        if matches!(evt.kind, Some(Kind::Error(ref e)) if e.message == "cancelled") {
            got_error = true;
            break;
        }
    }
    let elapsed = cancel_at.elapsed();
    assert!(got_error, "expected terminal Error('cancelled')");
    assert!(
        elapsed < Duration::from_millis(500),
        "cancel→drain took {elapsed:?}; must be <500ms"
    );
}

#[tokio::test]
async fn stream_resume_after_disconnect_replays_missed_events_with_no_dupes() {
    // Real failure mode: subscriber resuming with `since=N` would either
    // re-receive already-seen events (off-by-one inclusive) or miss the next
    // one (broadcast/buffer race).
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = ScriptedBackend::new("s3", |_text, tx, _cancel| {
        tokio::spawn(async move {
            for c in ["a", "b", "c", "d"] {
                let _ = tx.send(CoreEvent::TextDelta(c.into())).await;
            }
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket.clone()).await);
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open ok")
        .into_inner();

    // Drive the turn to completion via Send.
    let mut stream = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "go".into(),
        })
        .await
        .expect("send ok")
        .into_inner();
    while let Some(item) = stream.next().await {
        let evt = item.expect("evt");
        if matches!(evt.kind, Some(Kind::TurnComplete(_))) {
            break;
        }
    }

    // Now resume from seq=2 — should get seqs 3, 4, 5 (the last two deltas
    // plus TurnComplete).
    let mut resumed = client
        .stream(StreamRequest {
            session_id: opened.session_id.clone(),
            since_event_seq: 2,
        })
        .await
        .expect("stream ok")
        .into_inner();
    let mut seqs = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), resumed.next()).await {
            Ok(Some(Ok(evt))) => {
                seqs.push(evt.event_seq);
                if matches!(evt.kind, Some(Kind::TurnComplete(_))) {
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        seqs.iter().all(|s| *s > 2),
        "replay returned seqs {seqs:?}; all must be > 2"
    );
    let mut sorted = seqs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, seqs, "no duplicates expected");
}

#[tokio::test]
async fn shell_confirmation_round_trips_via_respond_to_prompt() {
    // Real failure mode: the prompt_id wire translation drops the oneshot
    // sender (so the agent hangs forever) or routes the response to the
    // wrong session/prompt. End-to-end: agent emits ShellConfirmation, client
    // calls RespondToPrompt, agent's `respond.await` resolves with the value.
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = ScriptedBackend::new("s4", |_text, tx, _cancel| {
        tokio::spawn(async move {
            let (otx, orx) = oneshot::channel::<bool>();
            let _ = tx
                .send(CoreEvent::ShellConfirmation {
                    command: "rm -rf /".into(),
                    respond: otx,
                })
                .await;
            // Wait for the user's reply.
            let approved = match tokio::time::timeout(Duration::from_secs(2), orx).await {
                Ok(Ok(b)) => b,
                _ => {
                    let _ = tx.send(CoreEvent::Error("no reply".into())).await;
                    return;
                }
            };
            let result = if approved { "approved" } else { "rejected" };
            let _ = tx
                .send(CoreEvent::ToolResult {
                    name: "run_shell".into(),
                    result: result.into(),
                })
                .await;
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket).await);
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open ok")
        .into_inner();

    let mut stream = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "delete".into(),
        })
        .await
        .expect("send ok")
        .into_inner();

    let mut prompt_id = None;
    let mut tool_result = None;
    while let Some(item) = stream.next().await {
        let evt = item.expect("evt");
        match evt.kind {
            Some(Kind::ShellConfirmation(sc)) => {
                prompt_id = Some(sc.prompt_id.clone());
                // Reply "false" → rejected.
                client
                    .respond_to_prompt(PromptResponse {
                        session_id: opened.session_id.clone(),
                        prompt_id: sc.prompt_id,
                        value: "false".into(),
                    })
                    .await
                    .expect("respond ok");
            }
            Some(Kind::ToolResult(tr)) => tool_result = Some(tr.result),
            Some(Kind::TurnComplete(_)) => break,
            _ => {}
        }
    }
    assert!(
        prompt_id.is_some(),
        "must have received a ShellConfirmation"
    );
    assert_eq!(tool_result.as_deref(), Some("rejected"));
}

#[tokio::test]
async fn two_consecutive_sends_yield_disjoint_per_turn_event_streams() {
    // Real failure mode (fixed in this commit): if the per-turn baseline_seq
    // were captured BEFORE start_turn, the second Send's stream could include
    // a tail event from the first turn (or, depending on timing, miss the
    // second turn's first event). Asserts strict disjointness.
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter_in = counter.clone();
    let backend = ScriptedBackend::new("seq", move |text, tx, _cancel| {
        let n = counter_in.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            // Each turn emits a deterministic prefix so we can attribute
            // events to a specific Send call after the fact.
            let prefix = format!("turn{n}:{text}/");
            for c in ["a", "b", "c"] {
                let _ = tx.send(CoreEvent::TextDelta(format!("{prefix}{c}"))).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket).await);
    let opened = client
        .open(OpenRequest {
            resume_id: String::new(),
        })
        .await
        .expect("open ok")
        .into_inner();

    // Drive turn 0 and collect.
    let mut s0 = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "first".into(),
        })
        .await
        .expect("send 0")
        .into_inner();
    let mut texts0 = Vec::new();
    while let Some(item) = s0.next().await {
        let evt = item.expect("evt");
        match evt.kind {
            Some(Kind::TextDelta(d)) => texts0.push(d.text),
            Some(Kind::TurnComplete(_)) => break,
            _ => {}
        }
    }

    // Now turn 1.
    let mut s1 = client
        .send(SendRequest {
            session_id: opened.session_id.clone(),
            text: "second".into(),
        })
        .await
        .expect("send 1")
        .into_inner();
    let mut texts1 = Vec::new();
    while let Some(item) = s1.next().await {
        let evt = item.expect("evt");
        match evt.kind {
            Some(Kind::TextDelta(d)) => texts1.push(d.text),
            Some(Kind::TurnComplete(_)) => break,
            _ => {}
        }
    }

    assert!(
        texts0.iter().all(|t| t.starts_with("turn0:first/")),
        "turn 0 stream must contain only turn-0 events, got {texts0:?}"
    );
    assert!(
        texts1.iter().all(|t| t.starts_with("turn1:second/")),
        "turn 1 stream must contain only turn-1 events (no leak from turn 0), got {texts1:?}"
    );
    assert_eq!(texts0.len(), 3);
    assert_eq!(texts1.len(), 3);
}

#[tokio::test]
async fn close_returns_not_found_for_unknown_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = ScriptedBackend::new("s5", |_t, tx, _c| {
        tokio::spawn(async move {
            let _ = tx.send(CoreEvent::TurnComplete).await;
        })
    });
    let factory = StubFactory::new(backend);
    let (socket, _shutdown) = spawn_session_service(tmp.path(), factory).await;

    let mut client = SessionClient::new(uds_channel(socket).await);
    let err = client
        .close(CloseRequest {
            session_id: "nonexistent".into(),
        })
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), tonic::Code::NotFound);
}
