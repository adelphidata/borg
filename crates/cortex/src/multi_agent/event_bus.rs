//! Session-scoped broadcast bus for sub-agent lifecycle events.
//!
//! Mirrors the [`crate::goals`] event bus pattern: a `tokio::sync::broadcast`
//! channel wrapped behind a thin publisher API, consumed by the gRPC
//! `SubscribeAgentTreeEvents` stream so the TUI and CLI can render live
//! spawn-tree updates without polling.
//!
//! The bus is bounded (256 slots). Lagged subscribers receive
//! `broadcast::error::RecvError::Lagged`, by which the TUI knows to re-issue
//! `ListAgents` for a fresh snapshot rather than continue from a corrupt
//! delta stream.
//!
//! **Single-publisher contract.** `seq` is allocated via `fetch_add` and then
//! the event is `send`'d as two separate steps. With one publisher (the
//! session's `AgentControl`), delivery order matches seq order. If a future
//! refactor wires multiple `AgentControl` instances to the same bus,
//! `(fetch_add, send)` would need to be serialized under a mutex to preserve
//! the strict-increasing guarantee subscribers rely on for gap detection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast;

use super::SubAgentInfo;

/// Capacity of the broadcast channel. Sized for bursts of spawn/complete
/// chatter on a tree with depth 5 and ~10 children per node.
const BUS_CAPACITY: usize = 256;

/// Discriminant for [`AgentTreeEvent`] kinds. Kept as a separate enum (not
/// just `Box<SubAgentStatus>`) because the bus also carries transition-only
/// events (`Spawned`, `Killed`) that don't map cleanly to a status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTreeEventKind {
    /// Agent was just spawned.
    Spawned,
    /// Agent transitioned to a non-terminal status (Running, PendingInit).
    StatusChanged,
    /// Agent entered Paused state.
    Paused,
    /// Agent left Paused state for Running.
    Resumed,
    /// Agent finished successfully.
    Completed,
    /// Agent errored.
    Errored,
    /// Agent was explicitly killed by an operator.
    Killed,
    /// Periodic liveness ping from a running child. Cadence configured by
    /// `agents.heartbeat_interval_secs`. TUI consumers surface this as a
    /// "still working" indicator; absence past `heartbeat_stale_cycles`
    /// triggers a `Stalled` event.
    Heartbeat,
    /// Child has gone past `heartbeat_stale_cycles × heartbeat_interval_secs`
    /// without producing any agent activity. Operator-actionable signal —
    /// either the LLM provider is wedged, a tool is hanging, or the child
    /// is genuinely waiting on something. Fires at most once per child.
    Stalled,
}

impl AgentTreeEventKind {
    /// Stable wire string for proto/JSON serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::StatusChanged => "status_changed",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Killed => "killed",
            Self::Heartbeat => "heartbeat",
            Self::Stalled => "stalled",
        }
    }
}

/// One lifecycle event. `seq` is monotonic across the bus's lifetime so a
/// reconnecting subscriber can detect gaps and force a snapshot reload.
#[derive(Debug, Clone)]
pub struct AgentTreeEvent {
    /// Event kind discriminant.
    pub kind: AgentTreeEventKind,
    /// Snapshot of the agent at the time the event was emitted.
    pub agent: SubAgentInfo,
    /// Monotonic sequence number (bus-scoped).
    pub seq: u64,
}

/// Cloneable publisher handle. The receiver side is created via [`Self::subscribe`].
#[derive(Debug, Clone)]
pub struct AgentTreeBus {
    tx: broadcast::Sender<AgentTreeEvent>,
    seq: Arc<AtomicU64>,
}

impl AgentTreeBus {
    /// Create a new bus.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Subscribe — caller receives every event published *after* the
    /// subscription. To reconcile with current state, callers should issue a
    /// `ListAgents` immediately after subscribing.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentTreeEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Allocates a new sequence number and broadcasts.
    /// Send failures (no subscribers) are silently dropped — the bus is
    /// observer-only, never a hard dependency of the agent loop.
    pub fn publish(&self, kind: AgentTreeEventKind, agent: &SubAgentInfo) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = AgentTreeEvent {
            kind,
            agent: agent.clone(),
            seq,
        };
        // `send` returns `Err(SendError)` only when there are no subscribers,
        // which is fine — events are observer-only.
        let _ = self.tx.send(event);
    }

    /// Current receiver count. Used by tests + diagnostics.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for AgentTreeBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::{SubAgentMetrics, SubAgentStatus};

    fn mk_info(id: &str, parent: &str) -> SubAgentInfo {
        SubAgentInfo {
            id: id.to_string(),
            nickname: format!("nick-{id}"),
            role: "test".to_string(),
            model: "test-model".to_string(),
            parent_session_id: parent.to_string(),
            session_id: format!("session-{id}"),
            depth: 1,
            status: SubAgentStatus::Running,
            created_at: 0,
            ended_at: None,
            metrics: SubAgentMetrics::default(),
        }
    }

    #[tokio::test]
    async fn seq_is_strictly_monotonic_across_kinds() {
        // Encoded invariant: subscribers rely on `seq` to detect gaps. If two
        // events ever shared a seq, the gap-detection in the TUI would miss
        // dropped events and silently corrupt the rendered tree.
        let bus = AgentTreeBus::new();
        let mut rx = bus.subscribe();
        let info = mk_info("a", "root");
        bus.publish(AgentTreeEventKind::Spawned, &info);
        bus.publish(AgentTreeEventKind::Paused, &info);
        bus.publish(AgentTreeEventKind::Resumed, &info);
        bus.publish(AgentTreeEventKind::Killed, &info);

        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        let e3 = rx.recv().await.unwrap();
        let e4 = rx.recv().await.unwrap();

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(e3.seq, 3);
        assert_eq!(e4.seq, 4);
        assert_eq!(e1.kind, AgentTreeEventKind::Spawned);
        assert_eq!(e4.kind, AgentTreeEventKind::Killed);
    }

    #[tokio::test]
    async fn lagged_subscriber_recovers_via_explicit_marker() {
        // The TUI handles `Err(Lagged(_))` by re-issuing `ListAgents`. This
        // test pins the contract that the broadcast channel actually surfaces
        // Lagged when capacity is exceeded — if a future bus refactor swaps
        // backends and silently drops events instead, this test fails.
        use tokio::sync::broadcast::error::RecvError;

        let bus = AgentTreeBus::new();
        let mut rx = bus.subscribe();
        let info = mk_info("x", "root");
        for _ in 0..(BUS_CAPACITY + 8) {
            bus.publish(AgentTreeEventKind::Spawned, &info);
        }

        match rx.recv().await {
            Err(RecvError::Lagged(n)) => assert!(n >= 8, "expected lag of at least 8, got {n}"),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_not_an_error() {
        // Observer-only contract: an agent loop must never break just because
        // no one is listening to the bus.
        let bus = AgentTreeBus::new();
        assert_eq!(bus.receiver_count(), 0);
        let info = mk_info("y", "root");
        bus.publish(AgentTreeEventKind::Spawned, &info);
        // No panic, no error returned (publish has no error surface).
    }
}
