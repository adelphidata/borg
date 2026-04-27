//! Integration test for `DaemonSessionExecutor` — the bridge between the
//! gateway's `SessionExecutor` trait and the daemon's per-session machinery.
//!
//! Real failure modes guarded:
//!   1. A regression that creates a fresh `SessionHost` for every inbound
//!      message (memory leak + lost conversation context). Asserted via
//!      `registry.live_ids().len() == 1` after two `start_turn` calls on the
//!      same `session_key`.
//!   2. A regression where `cancel(session_key)` no-ops on a busy session.
//!      Asserted by calling `cancel` mid-turn and watching the receiver
//!      close promptly with a terminal `Error` event.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use async_trait::async_trait;
use borgd::session::backend::{InlineImage, SessionBackend};
use borgd::session::{SessionHost, SessionRegistry};
use cortex::agent::AgentEvent;
use gateway::session_executor::{InboundTurn, SessionExecutor};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Stub backend: emits `deltas.len()` TextDeltas with `pause` between each,
/// then TurnComplete. Cancellation interrupts the loop and emits Error.
struct StubBackend {
    id: String,
    deltas: Vec<&'static str>,
    pause: Duration,
}

#[async_trait]
impl SessionBackend for StubBackend {
    async fn run_turn(
        &self,
        _text: String,
        _images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) {
        for d in &self.deltas {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = event_tx.send(AgentEvent::Error("cancelled".into())).await;
                    return;
                }
                _ = tokio::time::sleep(self.pause) => {}
            }
            if event_tx
                .send(AgentEvent::TextDelta((*d).into()))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }

    fn session_id(&self) -> &str {
        &self.id
    }
}

/// Helper: build a registry pre-populated with a `StubBackend` for `id`. The
/// `DaemonSessionExecutor` finds the host via `registry.get(...)` so its
/// internal `AgentBackend::new` path is never exercised — that path requires
/// a real DB + provider config and is covered by other tests.
fn registry_with_stub(id: &str, deltas: Vec<&'static str>, pause_ms: u64) -> SessionRegistry {
    let registry = SessionRegistry::new();
    let backend: Arc<dyn SessionBackend> = Arc::new(StubBackend {
        id: id.to_string(),
        deltas,
        pause: Duration::from_millis(pause_ms),
    });
    registry.insert(SessionHost::new(backend));
    registry
}

fn executor(registry: SessionRegistry) -> borgd::session::DaemonSessionExecutor {
    borgd::session::DaemonSessionExecutor::new(
        registry,
        cortex::config::Config::default(),
        cortex::telemetry::BorgMetrics::noop(),
    )
}

#[tokio::test]
async fn start_turn_streams_deltas_then_turn_complete() {
    let registry = registry_with_stub("alice:t1", vec!["hi", "there"], 1);
    let exec = executor(registry);

    let (mut rx, _cancel) = exec
        .start_turn(InboundTurn {
            session_key: "alice:t1".into(),
            text: "ping".into(),
            images: vec![],
        })
        .await
        .expect("start_turn");

    let mut deltas: Vec<String> = Vec::new();
    let mut saw_complete = false;
    while let Some(evt) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .ok()
        .flatten()
    {
        match evt {
            AgentEvent::TextDelta(d) => deltas.push(d),
            AgentEvent::TurnComplete => {
                saw_complete = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(deltas, vec!["hi".to_string(), "there".to_string()]);
    assert!(
        saw_complete,
        "TurnComplete must arrive before stream closes"
    );
}

#[tokio::test]
async fn cancel_aborts_in_flight_turn_promptly() {
    // Real failure mode: a regression where `executor.cancel` doesn't reach
    // the underlying turn's CancellationToken would let the turn run to
    // completion ignoring `/cancel`.
    let registry = registry_with_stub(
        "bob:t1",
        vec!["a", "b", "c", "d", "e"],
        200, // long enough that cancellation matters
    );
    let exec = executor(registry);

    let (mut rx, _cancel) = exec
        .start_turn(InboundTurn {
            session_key: "bob:t1".into(),
            text: "go".into(),
            images: vec![],
        })
        .await
        .expect("start_turn");

    // Let one delta land, then cancel.
    let _ = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    let cancel_at = std::time::Instant::now();
    assert!(exec.cancel("bob:t1").await, "cancel must report a hit");

    // Drain to terminal — must close promptly after cancel.
    let mut last: Option<AgentEvent> = None;
    while let Some(evt) = tokio::time::timeout(Duration::from_millis(800), rx.recv())
        .await
        .ok()
        .flatten()
    {
        last = Some(evt);
    }
    let elapsed = cancel_at.elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "cancel→close took {elapsed:?}; must be <1500ms"
    );
    assert!(
        matches!(last, Some(AgentEvent::Error(_))),
        "expected terminal Error after cancel"
    );
}

#[tokio::test]
async fn second_start_turn_reuses_existing_host() {
    // Real failure mode: a regression where each inbound creates a new
    // SessionHost (memory leak + lost history). Asserted via live_ids
    // staying at 1 after two start_turn calls for the same session_key.
    let registry = registry_with_stub("carol:t1", vec!["one"], 1);
    let exec = borgd::session::DaemonSessionExecutor::new(
        registry.clone(),
        cortex::config::Config::default(),
        cortex::telemetry::BorgMetrics::noop(),
    );

    // First turn
    let (mut rx1, _) = exec
        .start_turn(InboundTurn {
            session_key: "carol:t1".into(),
            text: "first".into(),
            images: vec![],
        })
        .await
        .expect("start_turn 1");
    while tokio::time::timeout(Duration::from_millis(500), rx1.recv())
        .await
        .ok()
        .flatten()
        .is_some()
    {}

    // Second turn (re-use). Since our stub only emits once, we don't care
    // about the events here — only that no new host is created.
    let _ = exec
        .start_turn(InboundTurn {
            session_key: "carol:t1".into(),
            text: "second".into(),
            images: vec![],
        })
        .await
        .expect("start_turn 2");

    assert_eq!(
        registry.live_ids().len(),
        1,
        "registry must hold exactly one host for the session_key"
    );
}

#[tokio::test]
async fn cancel_unknown_session_returns_false() {
    let registry = SessionRegistry::new();
    let exec = executor(registry);
    assert!(!exec.cancel("nobody").await);
}
