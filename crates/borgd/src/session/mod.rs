//! Per-session host that lifts the agent loop into the daemon.
//!
//! One [`SessionHost`] per `session_id` owns:
//! - the underlying [`SessionBackend`] (a real `Agent` in production, a stub
//!   in tests),
//! - a [`SequencedBuffer`] that fans out every event to all live subscribers
//!   AND retains a ring of recent events for resume,
//! - a [`PromptRegistry`] mapping `prompt_id` → parked oneshot::Sender so
//!   `Session.RespondToPrompt` can route a client reply back to the agent,
//! - a [`CancellationToken`] for the in-flight turn (refreshed each `Send`).
//!
//! The [`SessionRegistry`] is the daemon-wide map keyed by `session_id`. It's
//! the single state owned by the gRPC services; everything else hangs off it.

pub mod backend;
pub mod buffer;
pub mod convert;
pub mod prompts;

use anyhow::{anyhow, Result};
use backend::SessionBackend;
use borg_core::agent::AgentEvent;
use buffer::SequencedBuffer;
use prompts::PromptRegistry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;

/// One live conversation. Cheap-to-clone — wraps an `Arc<Inner>`.
#[derive(Clone)]
pub struct SessionHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    session_id: String,
    backend: Arc<dyn SessionBackend>,
    buffer: SequencedBuffer,
    prompts: Arc<PromptRegistry>,
    /// Cancellation token for the *current* in-flight turn (or a never-fired
    /// fresh token between turns). Replaced on every `Session.Send`.
    current_turn_cancel: AsyncMutex<CancellationToken>,
}

impl SessionHost {
    /// Wrap a backend in a host. Caller is responsible for inserting the
    /// returned host into the [`SessionRegistry`].
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            inner: Arc::new(HostInner {
                session_id: backend.session_id().to_string(),
                backend,
                buffer: SequencedBuffer::new(),
                prompts: Arc::new(PromptRegistry::default()),
                current_turn_cancel: AsyncMutex::new(CancellationToken::new()),
            }),
        }
    }

    /// Stable session id — used by the registry and as an event-stream key.
    pub fn id(&self) -> &str {
        &self.inner.session_id
    }

    /// Highest assigned `event_seq`, or 0 if no events have been pushed.
    pub fn last_event_seq(&self) -> u64 {
        self.inner.buffer.last_seq()
    }

    /// Snapshot of every buffered event with `event_seq > since`. Used by
    /// `Session.Stream` to bring a resuming subscriber up to the live cursor.
    pub fn replay_since(&self, since: u64) -> Vec<borg_proto::session::AgentEvent> {
        self.inner.buffer.replay_since(since)
    }

    /// Subscribe to new events from the broadcast channel.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<borg_proto::session::AgentEvent> {
        self.inner.buffer.subscribe()
    }

    /// Route a `RespondToPrompt` reply through the prompt registry.
    pub fn respond_to_prompt(&self, prompt_id: &str, value: &str) -> Result<()> {
        self.inner
            .prompts
            .respond(prompt_id, value)
            .map_err(|e| anyhow!(e))
    }

    /// Cancel the in-flight turn (if any). No-op when between turns.
    pub async fn cancel_turn(&self) {
        let token = self.inner.current_turn_cancel.lock().await.clone();
        token.cancel();
    }

    /// Drop every parked prompt — invoked when the session is being closed
    /// so any waiting agent task unblocks promptly.
    pub fn close(&self) {
        self.inner.prompts.clear();
    }

    /// Run a single turn. Returns immediately after spawning the worker; the
    /// caller observes events via [`Self::subscribe`] (and/or the buffer).
    /// Returns the `CancellationToken` controlling this turn.
    pub async fn start_turn(&self, text: String) -> CancellationToken {
        let cancel = CancellationToken::new();
        {
            let mut slot = self.inner.current_turn_cancel.lock().await;
            *slot = cancel.clone();
        }

        let inner = self.inner.clone();
        let cancel_for_task = cancel.clone();
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);

        // Forwarder: stamp + broadcast each core event as a proto event.
        let inner_fwd = inner.clone();
        tokio::spawn(async move {
            while let Some(evt) = event_rx.recv().await {
                let proto = convert::to_proto(evt, &inner_fwd.prompts);
                inner_fwd.buffer.push(proto);
            }
        });

        // Backend turn — runs concurrently with the forwarder above.
        tokio::spawn(async move {
            inner
                .backend
                .run_turn(text, event_tx, cancel_for_task)
                .await;
        });

        cancel
    }
}

/// Daemon-wide map of `session_id` → live `SessionHost`. Cheap-to-clone.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, SessionHost>>>,
}

impl SessionRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a host, replacing any prior entry with the same id (which would
    /// indicate the caller resumed a session that was still live — close the
    /// old host first to release pending prompts).
    pub fn insert(&self, host: SessionHost) {
        let id = host.id().to_string();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(prior) = guard.insert(id, host) {
            prior.close();
        }
    }

    /// Look up a session by id.
    pub fn get(&self, id: &str) -> Option<SessionHost> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(id).cloned()
    }

    /// Remove (and close) a session.
    pub fn remove(&self, id: &str) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(host) = guard.remove(id) {
            host.close();
            true
        } else {
            false
        }
    }

    /// Snapshot of live session ids — used by `Admin.ListSessions` for the
    /// in-memory view (DB-persisted sessions are listed separately).
    pub fn live_ids(&self) -> Vec<String> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use borg_proto::session::agent_event::Kind;
    use std::time::Duration;

    /// Stub backend: emits N TextDelta events then TurnComplete, sleeping
    /// `step_delay` between each so cancellation tests can interrupt.
    struct StubBackend {
        id: String,
        deltas: Vec<String>,
        step_delay: Duration,
    }

    #[async_trait]
    impl SessionBackend for StubBackend {
        async fn run_turn(
            &self,
            _text: String,
            event_tx: mpsc::Sender<AgentEvent>,
            cancel: CancellationToken,
        ) {
            for d in &self.deltas {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = event_tx.send(AgentEvent::Error("cancelled".into())).await;
                        return;
                    }
                    _ = tokio::time::sleep(self.step_delay) => {}
                }
                if event_tx
                    .send(AgentEvent::TextDelta(d.clone()))
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

    fn stub(id: &str, deltas: &[&str], delay_ms: u64) -> Arc<StubBackend> {
        Arc::new(StubBackend {
            id: id.into(),
            deltas: deltas.iter().map(ToString::to_string).collect(),
            step_delay: Duration::from_millis(delay_ms),
        })
    }

    async fn drain_until_terminal(
        rx: &mut tokio::sync::broadcast::Receiver<borg_proto::session::AgentEvent>,
    ) -> Vec<borg_proto::session::AgentEvent> {
        let mut out = Vec::new();
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            let term = convert::is_terminal(&evt);
            out.push(evt);
            if term {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn turn_streams_deltas_then_turn_complete_with_monotonic_seqs() {
        // Real failure mode: a regression that drops events from the
        // forwarder (e.g. swallows mpsc errors, miscounts seqs) would silently
        // truncate the visible turn. This test asserts the full proto sequence
        // arrives via subscribe() with strictly increasing seqs.
        let host = SessionHost::new(stub("s1", &["a", "b", "c"], 1));
        let mut rx = host.subscribe();
        host.start_turn("hi".into()).await;
        let evts = drain_until_terminal(&mut rx).await;
        let kinds: Vec<&str> = evts
            .iter()
            .map(|e| match &e.kind {
                Some(Kind::TextDelta(_)) => "delta",
                Some(Kind::TurnComplete(_)) => "done",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["delta", "delta", "delta", "done"]);
        let seqs: Vec<u64> = evts.iter().map(|e| e.event_seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "seqs must be strictly monotonic, got {seqs:?}"
        );
    }

    #[tokio::test]
    async fn cancel_turn_aborts_quickly_and_emits_error() {
        // Real failure mode: cancellation token not propagated → the turn
        // would run to completion ignoring `/cancel`.
        let host = SessionHost::new(stub("s2", &["a", "b", "c", "d", "e"], 200));
        let mut rx = host.subscribe();
        host.start_turn("go".into()).await;
        // Let one delta land, then cancel.
        let _ = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        let cancel_at = std::time::Instant::now();
        host.cancel_turn().await;
        let evts = drain_until_terminal(&mut rx).await;
        let elapsed = cancel_at.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel→drain took {elapsed:?}; must be <500ms"
        );
        let last = evts.last().expect("at least one event");
        assert!(
            matches!(&last.kind, Some(Kind::Error(e)) if e.message == "cancelled"),
            "expected terminal Error('cancelled'), got {:?}",
            last.kind
        );
    }

    #[tokio::test]
    async fn replay_since_returns_only_events_after_cursor() {
        // Real failure mode: a Stream subscriber resuming with `since=N` would
        // either re-receive already-seen events (off-by-one inclusive) or
        // miss the next one.
        let host = SessionHost::new(stub("s3", &["a", "b", "c"], 1));
        let mut rx = host.subscribe();
        host.start_turn("hi".into()).await;
        drain_until_terminal(&mut rx).await;
        let after_two = host.replay_since(2);
        let kinds: Vec<u64> = after_two.iter().map(|e| e.event_seq).collect();
        assert!(
            kinds.iter().all(|s| *s > 2),
            "replay_since(2) returned seqs {kinds:?} — all must be > 2"
        );
    }

    #[tokio::test]
    async fn registry_replaces_session_and_closes_prior_host() {
        let registry = SessionRegistry::new();
        let host_a = SessionHost::new(stub("dup", &["a"], 1));
        let host_b = SessionHost::new(stub("dup", &["b"], 1));
        registry.insert(host_a);
        registry.insert(host_b.clone());
        // Latest wins.
        let live = registry.get("dup").expect("present");
        assert!(Arc::ptr_eq(&live.inner, &host_b.inner));
    }

    #[tokio::test]
    async fn two_subscribers_see_identical_event_streams() {
        // Real failure mode: a unicast channel (instead of broadcast) would
        // mean a second client gets no events.
        let host = SessionHost::new(stub("s4", &["x", "y"], 1));
        let mut rx_a = host.subscribe();
        let mut rx_b = host.subscribe();
        host.start_turn("go".into()).await;
        let a = drain_until_terminal(&mut rx_a).await;
        let b = drain_until_terminal(&mut rx_b).await;
        let seqs_a: Vec<u64> = a.iter().map(|e| e.event_seq).collect();
        let seqs_b: Vec<u64> = b.iter().map(|e| e.event_seq).collect();
        assert_eq!(seqs_a, seqs_b);
        assert!(seqs_a.len() >= 3); // 2 deltas + TurnComplete
    }
}
