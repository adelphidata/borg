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
pub mod executor;
pub mod prompts;
#[cfg(feature = "test-support")]
pub mod scripted;

pub use executor::DaemonSessionExecutor;

use anyhow::{anyhow, Result};
use backend::SessionBackend;
use buffer::SequencedBuffer;
use cortex::agent::AgentEvent;
use prompts::PromptRegistry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
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
    /// All mutable per-turn state that must be swapped atomically when a new
    /// turn starts: cancel token + a join handle on the prior turn's
    /// forwarder. Held under one mutex so `start_turn` can serialize a new
    /// turn against the prior one's drain (no event-leak between turns).
    turn_state: AsyncMutex<TurnState>,
    /// `true` while a turn's events are streaming. Set when a turn installs
    /// its state in [`SessionHost::start_turn`]; cleared when that turn's
    /// forwarder drains. Read by [`SessionHost::is_busy`] so an internal/
    /// system turn can decline to interrupt a live turn.
    busy: AtomicBool,
}

struct TurnState {
    cancel: CancellationToken,
    /// Awaited at the start of the next turn so the prior forwarder finishes
    /// pushing its events into the buffer before this turn's baseline is
    /// captured. Without this, `Session.Send`'s "events for THIS turn" filter
    /// could either include prior-turn tail events or skip early-this-turn
    /// events, depending on scheduling.
    forwarder: Option<JoinHandle<()>>,
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
                turn_state: AsyncMutex::new(TurnState {
                    cancel: CancellationToken::new(),
                    forwarder: None,
                }),
                busy: AtomicBool::new(false),
            }),
        }
    }

    /// True while a turn is in flight (its events are still streaming). Used by
    /// the executor to decide whether an internal/system turn may run now or
    /// must be deferred to avoid aborting a live turn.
    pub fn is_busy(&self) -> bool {
        self.inner.busy.load(Ordering::SeqCst)
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
    pub fn replay_since(&self, since: u64) -> Vec<protocols::session::AgentEvent> {
        self.inner.buffer.replay_since(since)
    }

    /// Subscribe to new events from the broadcast channel.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<protocols::session::AgentEvent> {
        self.inner.buffer.subscribe()
    }

    /// Push an out-of-band [`SystemNotice`](protocols::session::SystemNotice)
    /// event into the session's buffer so live subscribers and replayers see
    /// it as a system cell. Used by daemon-level emitters (posture
    /// transitions, budget alerts) that aren't part of any LLM turn.
    pub fn push_system_notice(&self, category: impl Into<String>, text: impl Into<String>) -> u64 {
        use protocols::session as pb;
        let evt = pb::AgentEvent {
            event_seq: 0,
            kind: Some(pb::agent_event::Kind::SystemNotice(pb::SystemNotice {
                category: category.into(),
                text: text.into(),
            })),
        };
        self.inner.buffer.push(evt)
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
        let token = self.inner.turn_state.lock().await.cancel.clone();
        token.cancel();
    }

    /// Cancel the in-flight turn AND drop every parked prompt. Invoked when
    /// the session is being closed or replaced so any waiting agent task
    /// unblocks promptly and the next turn (if any) starts clean.
    pub async fn close(&self) {
        let token = self.inner.turn_state.lock().await.cancel.clone();
        token.cancel();
        self.inner.prompts.clear();
        if let Err(e) = self.inner.backend.close_session().await {
            tracing::warn!(
                session_id = %self.inner.session_id,
                error = %e,
                "session backend close failed"
            );
        }
    }

    /// Compact the agent's conversation. Forwards to the backend.
    pub async fn compact(&self) -> Result<(usize, usize)> {
        self.inner.backend.compact().await
    }

    /// Wipe history (keeps the agent alive for new turns).
    pub fn clear_history(&self) -> Result<()> {
        self.inner.backend.clear_history()
    }

    /// Drop the most recent assistant turn.
    pub fn undo(&self) -> Result<usize> {
        self.inner.backend.undo()
    }

    /// Rewind to Nth user message.
    pub fn rewind_to_nth_user_message(&self, n: usize) -> Result<usize> {
        self.inner.backend.rewind_to_nth_user_message(n)
    }

    /// Persist current agent state.
    pub fn auto_save(&self) -> Result<()> {
        self.inner.backend.auto_save()
    }

    /// Start a fresh agent session in this host.
    pub async fn new_agent_session(&self) -> Result<String> {
        self.inner.backend.new_agent_session().await
    }

    /// Load an existing agent session in this host.
    pub fn load_agent_session(&self, id: &str) -> Result<u64> {
        self.inner.backend.load_agent_session(id)
    }

    /// Snapshot of agent-visible state for `Session.GetState`.
    pub fn snapshot_state(&self) -> Result<backend::SessionStateSnapshot> {
        self.inner.backend.snapshot_state()
    }

    /// Push a steer message into the in-flight turn.
    pub fn steer(&self, text: &str) -> Result<()> {
        self.inner.backend.steer(text)
    }

    /// Mirror a system-injected assistant message into the agent's
    /// history. Used by the daemon to bridge proactive deliveries
    /// (heartbeats) into the same session the user is replying in,
    /// so the agent has the context for follow-up turns.
    pub async fn append_assistant_message(&self, text: &str) -> Result<()> {
        self.inner.backend.append_assistant_message(text).await
    }

    /// Fork the active agent session at its last message. Forwards to the
    /// backend; daemon-side execution is required because SQLite has a single
    /// writer (the daemon) and the TUI cannot open a writer handle without
    /// racing it.
    pub async fn fork(&self) -> Result<backend::ForkOutcome> {
        self.inner.backend.fork().await
    }

    /// Start a turn. Awaits the prior turn's forwarder draining into the
    /// buffer before installing this turn's state — that's how we guarantee
    /// "events with seq > baseline_after_start belong to this turn". Returns
    /// `(cancel_token, baseline_seq_at_start)`. Subscribe to the buffer
    /// before calling and filter `evt.event_seq > baseline_seq_at_start`.
    ///
    /// `deadline` is an optional wall-clock instant by which the caller
    /// expects the turn to have surfaced any structured outcome (typically
    /// derived from a gRPC `Send` request's client deadline). When set, a
    /// watcher fires `LLM_DEADLINE_SAFETY_MARGIN_MS` before the deadline
    /// — it pushes an `AgentEvent::Error` describing the impending timeout
    /// onto the event stream and then cancels the in-flight turn so the
    /// client sees a structured `Error` event instead of a bare gRPC
    /// `DEADLINE_EXCEEDED`.
    pub async fn start_turn(
        &self,
        text: String,
        images: Vec<backend::InlineImage>,
        text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
        deadline: Option<std::time::Instant>,
    ) -> (CancellationToken, u64) {
        let cancel = CancellationToken::new();
        let inner = self.inner.clone();
        let cancel_for_task = cancel.clone();
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);

        // Forwarder: stamp + broadcast each core event as a proto event. The
        // task exits when ALL `event_tx` clones drop — see `AgentBackend`'s
        // `run_turn` for the senders that hold this open.
        let inner_fwd = inner.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(evt) = event_rx.recv().await {
                let proto = convert::to_proto(evt, &inner_fwd.prompts);
                inner_fwd.buffer.push(proto);
            }
            // Turn fully streamed → session is idle again. This MUST remain the
            // FINAL statement of the forwarder: the next turn's `start_turn`
            // awaits this task (`prior.await`) before it sets `busy = true`, so
            // anything appended after this store would let `prior.await` resolve
            // while `busy` is momentarily stale — reopening a window where
            // `is_busy()` wrongly reports idle during a live turn.
            inner_fwd.busy.store(false, Ordering::SeqCst);
        });

        // Critical section: await prior forwarder's drain, install our turn
        // state, capture baseline seq — all under the same mutex so a
        // concurrent `Send`/`Cancel`/`close` can't observe a half-installed
        // state. The await on `prior_forwarder` may block if the previous
        // turn's backend is still running; that's intentional — turns are
        // serialized per session.
        let baseline = {
            let mut state = inner.turn_state.lock().await;
            if let Some(prior) = state.forwarder.take() {
                if let Err(e) = prior.await {
                    tracing::warn!(error = %e, "prior turn's forwarder join failed");
                }
            }
            state.cancel = cancel.clone();
            state.forwarder = Some(forwarder);
            // Mark busy under the same lock that installs the turn, so a
            // concurrent `is_busy()` can't observe a half-installed state.
            inner.busy.store(true, Ordering::SeqCst);
            inner.buffer.last_seq()
        };

        let event_tx_for_backend = event_tx.clone();
        let session_id_for_watcher = inner.session_id.clone();
        let cancel_for_deadline = cancel.clone();

        // Backend turn — runs concurrently with the forwarder above.
        let backend_handle = tokio::spawn(async move {
            inner
                .backend
                .run_turn(
                    text,
                    images,
                    event_tx_for_backend,
                    cancel_for_task,
                    text_file_bridge,
                )
                .await;
        });

        if let Some(deadline) = deadline {
            spawn_deadline_watcher(
                deadline,
                event_tx,
                cancel_for_deadline,
                session_id_for_watcher,
                backend_handle,
            );
        } else {
            drop(event_tx);
        }

        (cancel, baseline)
    }
}

/// Spawn a task that surfaces an `AgentEvent::Error` and cancels the in-flight
/// turn `LLM_DEADLINE_SAFETY_MARGIN_MS` before `deadline` fires. If the backend
/// task finishes before the deadline, the watcher exits silently without
/// touching the stream — `backend_handle` is the join handle on the backend's
/// `run_turn` task, so we observe completion regardless of how the agent
/// terminated (TurnComplete, Error, panic).
///
/// IMPORTANT: this task owns one of the two `event_tx` clones; the other
/// lives inside the backend task. The forwarder loop in [`SessionHost::start_turn`]
/// terminates only when BOTH senders drop, and the next turn's `start_turn`
/// blocks on `prior.await` until that drain completes. Every termination arm
/// below must therefore drop `event_tx` (which it does implicitly on `return`
/// / end-of-async-block). Do not refactor to clone-and-pre-drop — that would
/// break the prior-turn ordering invariant in `start_turn`.
fn spawn_deadline_watcher(
    deadline: std::time::Instant,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    session_id: String,
    backend_handle: JoinHandle<()>,
) {
    use cortex::constants::LLM_DEADLINE_SAFETY_MARGIN_MS;
    // Scale the safety margin to the remaining budget. With a generous
    // 120s budget the full 5s margin applies (fire at t≈115s). With a tight
    // 200ms budget the margin shrinks to ≤budget/2 so the watcher still
    // sleeps half the budget before firing — otherwise `sleep(0)` wins the
    // select! arm immediately, even when the backend was about to complete
    // in microseconds, and every fast turn would be tagged with a spurious
    // "deadline approaching" Error.
    let now_at_spawn = std::time::Instant::now();
    let total_budget = deadline.saturating_duration_since(now_at_spawn);
    let configured_margin = std::time::Duration::from_millis(LLM_DEADLINE_SAFETY_MARGIN_MS);
    let effective_margin = configured_margin.min(total_budget / 2);
    let trigger_at = deadline.checked_sub(effective_margin).unwrap_or(deadline);
    let total_budget_ms = u64::try_from(total_budget.as_millis()).unwrap_or(u64::MAX);
    tokio::spawn(async move {
        let sleep = trigger_at.saturating_duration_since(std::time::Instant::now());
        tokio::pin!(backend_handle);
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = cancel.cancelled() => {
                // Turn was cancelled by some other path (user cancel, close).
                // Don't fire — let whoever cancelled own the message.
                return;
            }
            _ = &mut backend_handle => {
                // Backend finished naturally before the deadline. The agent
                // already emitted its terminal event (TurnComplete or Error);
                // injecting another Error would be a UX regression.
                return;
            }
        }
        let effective_margin_ms = u64::try_from(effective_margin.as_millis()).unwrap_or(u64::MAX);
        let msg = format!(
            "turn aborted: client deadline approaching (total budget {total_budget_ms}ms, \
             effective margin {effective_margin_ms}ms) — LLM call did not surface a \
             response in time"
        );
        tracing::warn!(
            session_id = %session_id,
            total_budget_ms,
            effective_margin_ms,
            "deadline watcher firing"
        );
        if let Err(e) = event_tx.send(AgentEvent::Error(msg)).await {
            // Forwarder already drained and dropped its receiver — race between
            // watcher and turn completion. Nothing to recover.
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "deadline watcher failed to surface Error event (turn already finished?)"
            );
        }
        cancel.cancel();
    });
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
    /// indicate the caller resumed a session that was still live — cancel the
    /// old host's in-flight turn and clear its pending prompts in the
    /// background so this call stays sync).
    pub fn insert(&self, host: SessionHost) {
        let id = host.id().to_string();
        let prior = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.insert(id, host)
        };
        if let Some(prior) = prior {
            tokio::spawn(async move { prior.close().await });
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

    /// Remove and return a session host.
    pub fn remove_host(&self, id: &str) -> Option<SessionHost> {
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(id)
        }
    }

    /// Broadcast a system notice to every live session. Caller is responsible
    /// for the rate limit on the upstream emitter (posture: 2/hr at the audit
    /// chain layer); this method is unconditional and synchronous. Returns
    /// the number of sessions that received the notice.
    pub fn broadcast_system_notice(&self, category: &str, text: &str) -> usize {
        let hosts: Vec<SessionHost> = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.values().cloned().collect()
        };
        let n = hosts.len();
        for h in hosts {
            h.push_system_notice(category, text);
        }
        n
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
    use protocols::session::agent_event::Kind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Stub backend: emits N TextDelta events then TurnComplete, sleeping
    /// `step_delay` between each so cancellation tests can interrupt.
    struct StubBackend {
        id: String,
        deltas: Vec<String>,
        step_delay: Duration,
        close_calls: Arc<AtomicUsize>,
        append_calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl SessionBackend for StubBackend {
        async fn run_turn(
            &self,
            _text: String,
            _images: Vec<backend::InlineImage>,
            event_tx: mpsc::Sender<AgentEvent>,
            cancel: CancellationToken,
            _text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
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

        async fn close_session(&self) -> Result<()> {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn append_assistant_message(&self, text: &str) -> anyhow::Result<()> {
            self.append_calls.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn stub(id: &str, deltas: &[&str], delay_ms: u64) -> Arc<StubBackend> {
        Arc::new(StubBackend {
            id: id.into(),
            deltas: deltas.iter().map(ToString::to_string).collect(),
            step_delay: Duration::from_millis(delay_ms),
            close_calls: Arc::new(AtomicUsize::new(0)),
            append_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    async fn wait_for_close_calls(counter: &AtomicUsize, expected: usize) {
        let start = tokio::time::Instant::now();
        loop {
            if counter.load(Ordering::SeqCst) == expected {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "timed out waiting for close_calls={expected}, got {}",
                counter.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn drain_until_terminal(
        rx: &mut tokio::sync::broadcast::Receiver<protocols::session::AgentEvent>,
    ) -> Vec<protocols::session::AgentEvent> {
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
        host.start_turn("hi".into(), Vec::new(), None, None).await;
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
        host.start_turn("go".into(), Vec::new(), None, None).await;
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
    async fn is_busy_tracks_turn_lifecycle() {
        // Real failure mode: if `busy` is never set (or cleared too early), the
        // executor's internal-turn deferral can't tell a live turn from an idle
        // session — and would either always interrupt or never defer.
        let host = SessionHost::new(stub("s-busy", &["a", "b"], 50));
        assert!(!host.is_busy(), "must be idle before any turn");

        let mut rx = host.subscribe();
        host.start_turn("go".into(), Vec::new(), None, None).await;
        assert!(host.is_busy(), "must be busy while a turn is in flight");

        drain_until_terminal(&mut rx).await;
        // The forwarder clears `busy` after the terminal event drains — allow a
        // few ticks for that task to run.
        for _ in 0..100 {
            if !host.is_busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !host.is_busy(),
            "must be idle again after the turn completes"
        );
    }

    #[tokio::test]
    async fn replay_since_returns_only_events_after_cursor() {
        // Real failure mode: a Stream subscriber resuming with `since=N` would
        // either re-receive already-seen events (off-by-one inclusive) or
        // miss the next one.
        let host = SessionHost::new(stub("s3", &["a", "b", "c"], 1));
        let mut rx = host.subscribe();
        host.start_turn("hi".into(), Vec::new(), None, None).await;
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
        let backend_a = stub("dup", &["a"], 1);
        let close_calls = backend_a.close_calls.clone();
        let host_a = SessionHost::new(backend_a);
        let host_b = SessionHost::new(stub("dup", &["b"], 1));
        registry.insert(host_a);
        registry.insert(host_b.clone());
        // Latest wins.
        let live = registry.get("dup").expect("present");
        assert!(Arc::ptr_eq(&live.inner, &host_b.inner));
        wait_for_close_calls(&close_calls, 1).await;
    }

    #[tokio::test]
    async fn two_subscribers_see_identical_event_streams() {
        // Real failure mode: a unicast channel (instead of broadcast) would
        // mean a second client gets no events.
        let host = SessionHost::new(stub("s4", &["x", "y"], 1));
        let mut rx_a = host.subscribe();
        let mut rx_b = host.subscribe();
        host.start_turn("go".into(), Vec::new(), None, None).await;
        let a = drain_until_terminal(&mut rx_a).await;
        let b = drain_until_terminal(&mut rx_b).await;
        let seqs_a: Vec<u64> = a.iter().map(|e| e.event_seq).collect();
        let seqs_b: Vec<u64> = b.iter().map(|e| e.event_seq).collect();
        assert_eq!(seqs_a, seqs_b);
        assert!(seqs_a.len() >= 3); // 2 deltas + TurnComplete
    }

    #[tokio::test]
    async fn registry_broadcast_system_notice_reaches_every_live_session() {
        // Real failure mode: posture transition fires in the audit chain but
        // open clients never see the toast — UX promise broken.
        let registry = SessionRegistry::new();
        let host_a = SessionHost::new(stub("s-a", &[], 1));
        let host_b = SessionHost::new(stub("s-b", &[], 1));
        let mut rx_a = host_a.subscribe();
        let mut rx_b = host_b.subscribe();
        registry.insert(host_a);
        registry.insert(host_b);

        let n =
            registry.broadcast_system_notice("posture", "Default \u{2192} Boosted (\u{00d7}1.10)");
        assert_eq!(n, 2, "broadcast must reach every live session");

        for rx in [&mut rx_a, &mut rx_b] {
            let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("notice timeout")
                .expect("recv");
            match evt.kind.expect("kind") {
                Kind::SystemNotice(n) => {
                    assert_eq!(n.category, "posture");
                    assert!(n.text.contains("Boosted"), "text: {}", n.text);
                }
                other => panic!("expected SystemNotice, got {other:?}"),
            }
            assert!(evt.event_seq > 0, "system notice must be sequenced");
        }
    }

    #[tokio::test]
    async fn host_close_finalizes_backend_once() {
        let backend = stub("s-close", &[], 1);
        let close_calls = backend.close_calls.clone();
        let host = SessionHost::new(backend);

        host.close().await;
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);

        host.close().await;
        assert_eq!(close_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn registry_remove_host_returns_host_for_awaited_close() {
        let registry = SessionRegistry::new();
        let backend = stub("s-remove", &[], 1);
        let close_calls = backend.close_calls.clone();
        let host = SessionHost::new(backend);
        registry.insert(host);

        let removed = registry.remove_host("s-remove").expect("removed host");
        assert!(
            registry.get("s-remove").is_none(),
            "host must be removed first"
        );
        removed.close().await;

        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn host_close_cancels_turn_and_finalizes_backend() {
        let backend = stub("s-close-active", &["a", "b", "c", "d"], 200);
        let close_calls = backend.close_calls.clone();
        let host = SessionHost::new(backend);
        let mut rx = host.subscribe();
        host.start_turn("go".into(), Vec::new(), None, None).await;

        let _ = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        host.close().await;
        let evts = drain_until_terminal(&mut rx).await;

        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        let last = evts.last().expect("at least one event");
        assert!(
            matches!(&last.kind, Some(Kind::Error(e)) if e.message == "cancelled"),
            "expected terminal Error('cancelled'), got {:?}",
            last.kind
        );
    }

    #[tokio::test]
    async fn deadline_watcher_surfaces_error_before_deadline_when_turn_stalls() {
        // Real failure mode (smorgasborg session-send-...05-09 ticket): a hung
        // LLM call inside the agent loop runs out the gRPC client's deadline
        // with no event on the wire — the client sees a bare DEADLINE_EXCEEDED
        // and the borg-side `Error` event never reaches it. The deadline
        // watcher must inject a structured `Error` event onto the stream
        // *before* the wall-clock deadline so the client knows what happened.
        //
        // We use a stub that runs ~5s of "deltas" (well past our 200ms test
        // budget). With LLM_DEADLINE_SAFETY_MARGIN_MS = 5_000, a 200ms-future
        // deadline saturates the trigger_at to `now`, so the watcher fires
        // immediately and surfaces an Error within milliseconds — well before
        // the 200ms wall-clock deadline.
        let host = SessionHost::new(stub("s-deadline", &["a", "b", "c", "d", "e"], 1_000));
        let mut rx = host.subscribe();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let started = std::time::Instant::now();
        host.start_turn("hang".into(), Vec::new(), None, Some(deadline))
            .await;

        // Drain until we see a terminal event. Bound the wait at 1s — way more
        // than the 200ms deadline so a regression fails loudly.
        let mut saw_deadline_error = false;
        loop {
            let evt = match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(e)) => e,
                Ok(Err(_)) | Err(_) => break,
            };
            if let Some(Kind::Error(e)) = &evt.kind {
                if e.message.contains("client deadline approaching") {
                    saw_deadline_error = true;
                }
            }
            if convert::is_terminal(&evt) {
                break;
            }
        }
        let elapsed = started.elapsed();
        assert!(
            saw_deadline_error,
            "expected an Error event mentioning the client deadline; never saw one in {elapsed:?}"
        );
        // Watcher must beat the actual deadline plus a generous slack for CI.
        assert!(
            elapsed < Duration::from_millis(500),
            "deadline watcher took {elapsed:?}; must surface Error well before the 200ms deadline"
        );
    }

    #[tokio::test]
    async fn deadline_watcher_stays_silent_when_user_cancels_before_deadline() {
        // Real failure mode: if the watcher fires on the cancel arm, the user
        // would see two terminal Errors back-to-back ("cancelled" from the
        // backend + "client deadline approaching" from the watcher). Spec:
        // whoever cancelled owns the message. The cancel arm of the
        // watcher's select! must `return` without touching the stream.
        let host = SessionHost::new(stub("s-cancel", &["a", "b", "c", "d", "e"], 200));
        let mut rx = host.subscribe();
        // Generous 5s deadline — well past the user-cancel that follows.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        host.start_turn("go".into(), Vec::new(), None, Some(deadline))
            .await;
        // Let one delta land so the turn is genuinely in flight.
        let _ = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        host.cancel_turn().await;
        let evts = drain_until_terminal(&mut rx).await;
        let last = evts.last().expect("at least one event");
        assert!(
            matches!(&last.kind, Some(Kind::Error(e)) if e.message == "cancelled"),
            "expected terminal Error('cancelled') from backend, got {:?}",
            last.kind
        );
        let deadline_msgs: Vec<_> = evts
            .iter()
            .filter_map(|e| match &e.kind {
                Some(Kind::Error(err)) if err.message.contains("client deadline approaching") => {
                    Some(err.message.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            deadline_msgs.is_empty(),
            "user-cancelled turn must not also receive a deadline Error, but saw {deadline_msgs:?}"
        );
    }

    #[tokio::test]
    async fn deadline_watcher_scales_safety_margin_to_tight_budgets() {
        // Real failure mode: if the watcher used the configured 5s margin
        // unconditionally, every turn under a budget shorter than 10s would
        // fire `sleep(0)` immediately and tag the turn with a deadline Error
        // before the backend had any chance to make progress. The scaling
        // rule `effective_margin = min(configured, total_budget / 2)` must
        // hold so a stalled turn under a 200ms budget fires somewhere in
        // [50ms, 150ms] — i.e. NOT instantly, AND well before the deadline.
        let host = SessionHost::new(stub("s-scale", &["a", "b", "c", "d"], 1_000));
        let mut rx = host.subscribe();
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(200);
        host.start_turn("hang".into(), Vec::new(), None, Some(deadline))
            .await;
        // Find the deadline Error and pin its arrival time.
        let mut fire_at: Option<std::time::Instant> = None;
        loop {
            let evt = match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(e)) => e,
                _ => break,
            };
            if let Some(Kind::Error(e)) = &evt.kind {
                if e.message.contains("client deadline approaching") {
                    fire_at = Some(std::time::Instant::now());
                }
            }
            if convert::is_terminal(&evt) {
                break;
            }
        }
        let fire_at = fire_at.expect("watcher must fire a deadline Error under a stalled turn");
        let elapsed = fire_at.duration_since(started);
        // Lower bound: scaling must give the backend at least ~budget/2 to
        // make progress (50ms is generous slack against scheduler jitter).
        assert!(
            elapsed >= Duration::from_millis(50),
            "watcher fired at {elapsed:?}; must wait at least ~budget/2 (≥50ms) before firing"
        );
        // Upper bound: must beat the actual 200ms deadline with margin.
        assert!(
            elapsed < Duration::from_millis(180),
            "watcher fired at {elapsed:?}; must beat the 200ms client deadline"
        );
    }

    #[tokio::test]
    async fn deadline_watcher_stays_silent_when_turn_completes_in_time() {
        // Inverse of the prior test: if the turn finishes well before the
        // deadline, the watcher must not inject a spurious Error event.
        // Otherwise every fast turn would carry a confusing "deadline
        // approaching" tail event.
        let host = SessionHost::new(stub("s-fast", &["x"], 1));
        let mut rx = host.subscribe();
        // Generous 5s deadline — turn completes in milliseconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        host.start_turn("fast".into(), Vec::new(), None, Some(deadline))
            .await;
        let evts = drain_until_terminal(&mut rx).await;
        let kinds: Vec<_> = evts
            .iter()
            .filter_map(|e| match &e.kind {
                Some(Kind::Error(err)) => Some(err.message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            kinds.is_empty(),
            "fast turn must not get a deadline Error event, but saw {kinds:?}"
        );
        let last = evts.last().expect("at least one event");
        assert!(
            matches!(&last.kind, Some(Kind::TurnComplete(_))),
            "expected TurnComplete, got {:?}",
            last.kind
        );
    }

    #[tokio::test]
    async fn append_assistant_message_reaches_backend() {
        // Pinned by Bug B: a heartbeat the daemon streamed to the TUI must
        // also be mirrored into the active session's history, so the user's
        // next reply has it in context. The mirror path goes through the
        // host wrapper; this test asserts the call is delegated to the
        // backend with the exact text it was given.
        let backend = stub("s-mirror", &[], 1);
        let append_calls = backend.append_calls.clone();
        let host = SessionHost::new(backend);

        host.append_assistant_message("[heartbeat] 2 task runs failed")
            .await
            .expect("append must succeed");
        host.append_assistant_message("follow-up text")
            .await
            .expect("append must succeed");

        let recorded = append_calls.lock().unwrap();
        assert_eq!(
            recorded.as_slice(),
            &[
                "[heartbeat] 2 task runs failed".to_string(),
                "follow-up text".to_string()
            ],
            "host must forward each call verbatim to the backend"
        );
    }
}
