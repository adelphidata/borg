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
use gateway::session_executor::{InboundTurn, SessionExecutor, TurnKind};
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
        _text_file_bridge: Option<std::sync::Arc<dyn cortex::execution_context::TextFileBridge>>,
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
        None,
        None,
        None,
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
            kind: TurnKind::User,
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
            kind: TurnKind::User,
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
        None,
        None,
        None,
    );

    // First turn
    let (mut rx1, _) = exec
        .start_turn(InboundTurn {
            session_key: "carol:t1".into(),
            text: "first".into(),
            images: vec![],
            kind: TurnKind::User,
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
            kind: TurnKind::User,
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
async fn internal_turn_does_not_interrupt_busy_user_turn() {
    // Core invariant: an internal/system turn must NEVER abort a user's
    // in-flight turn. Regression guard for the unconditional `cancel_turn()`
    // that used to run for every inbound in DaemonSessionExecutor::start_turn.
    let registry = registry_with_stub("dave:t1", vec!["a", "b", "c", "d"], 50);
    let exec = executor(registry.clone());

    let (mut user_rx, _c) = exec
        .start_turn(InboundTurn {
            session_key: "dave:t1".into(),
            text: "work".into(),
            images: vec![],
            kind: TurnKind::User,
        })
        .await
        .expect("user start_turn");

    // Wait for the first delta so the turn is genuinely in flight (busy).
    let first = tokio::time::timeout(Duration::from_secs(1), user_rx.recv())
        .await
        .expect("first delta in time")
        .expect("a delta");
    assert!(matches!(first, AgentEvent::TextDelta(_)));

    // Fire an INTERNAL turn at the same session while it's busy.
    let (mut internal_rx, _ic) = exec
        .start_turn(InboundTurn {
            session_key: "dave:t1".into(),
            text: "heartbeat".into(),
            images: vec![],
            kind: TurnKind::Internal,
        })
        .await
        .expect("internal start_turn");

    // Deferred: the internal stream closes immediately with no events.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), internal_rx.recv())
            .await
            .expect("internal stream resolves promptly")
            .is_none(),
        "deferred internal turn must yield an empty, closed stream"
    );

    // The user turn must run to completion uninterrupted — no 'cancelled'.
    let mut got_complete = false;
    let mut cancelled = false;
    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(2), user_rx.recv()).await {
        match evt {
            AgentEvent::TurnComplete => {
                got_complete = true;
                break;
            }
            AgentEvent::Error(e) if e == "cancelled" => {
                cancelled = true;
                break;
            }
            _ => {}
        }
    }
    assert!(!cancelled, "internal turn must not cancel the user turn");
    assert!(got_complete, "user turn must complete normally");
    assert_eq!(registry.live_ids().len(), 1, "no extra host created");
}

#[tokio::test]
async fn user_turn_still_replaces_busy_user_turn() {
    // Contrast to the internal case: a new USER message preserves today's
    // "latest message wins" semantics and cancels the prior in-flight turn.
    let registry = registry_with_stub("erin:t1", vec!["a", "b", "c", "d"], 50);
    let exec = executor(registry);

    let (mut rx1, _c) = exec
        .start_turn(InboundTurn {
            session_key: "erin:t1".into(),
            text: "first".into(),
            images: vec![],
            kind: TurnKind::User,
        })
        .await
        .expect("first start_turn");
    let _ = tokio::time::timeout(Duration::from_secs(1), rx1.recv()).await;

    let (_rx2, _c2) = exec
        .start_turn(InboundTurn {
            session_key: "erin:t1".into(),
            text: "second".into(),
            images: vec![],
            kind: TurnKind::User,
        })
        .await
        .expect("second start_turn");

    let mut saw_cancel = false;
    while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(2), rx1.recv()).await {
        if matches!(&evt, AgentEvent::Error(e) if e == "cancelled") {
            saw_cancel = true;
            break;
        }
    }
    assert!(
        saw_cancel,
        "a new user turn must cancel the prior in-flight turn"
    );
}

#[tokio::test]
async fn cancel_unknown_session_returns_false() {
    let registry = SessionRegistry::new();
    let exec = executor(registry);
    assert!(!exec.cancel("nobody").await);
}
