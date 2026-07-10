//! Broadcast bus for `cortex::goals` lifecycle events.
//!
//! Producers (the agent loop running inside the daemon, and the four
//! `Admin.{Get,Set,Clear,PauseGoalOnInterrupt}Goal` RPC handlers) publish
//! `DaemonGoalEvent`s through this bus. Subscribers
//! (`Admin.SubscribeGoalEvents` stream consumers — the TUI footer today,
//! replicated dashboards later) receive a real-time feed without polling the
//! SQLite row.
//!
//! Design mirrors [`crate::heartbeat::HeartbeatBus`]: a cloneable wrapper
//! over `tokio::sync::broadcast::Sender`. `send` is fire-and-forget — when
//! there are no subscribers (steady state before the TUI opens), the error
//! is intentionally swallowed because the events are pure UI notifications
//! and replaying them on the next `GetGoal` re-syncs state.

use cortex::goals::{Goal, GoalEventSink};
use std::sync::Arc;
use tokio::sync::broadcast;

/// A single lifecycle event. `goal` is `None` only for `kind="cleared"`
/// (the row was deleted); `Some` otherwise — the source paths re-fetch the
/// row before emitting so subscribers always observe the canonical state.
#[derive(Clone, Debug)]
pub struct DaemonGoalEvent {
    /// Session whose goal changed.
    pub session_id: String,
    /// Lifecycle kind — one of [`cortex::goals::event_kind`] constants.
    pub kind: String,
    /// Post-mutation snapshot of the goal row, or `None` for `cleared`.
    pub goal: Option<Goal>,
    /// Server-side emit time (unix seconds). Used by clients to discard
    /// stale events on reconnect rather than re-applying them.
    pub emitted_at_unix: i64,
}

/// Broadcast bus. Cheap to clone — wraps a `broadcast::Sender`. Capacity 32
/// matches `HeartbeatBus` and is deep enough that a momentarily-slow TUI
/// won't be marked `Lagged` on a burst of accounting transitions.
#[derive(Clone)]
pub struct GoalEventBus {
    tx: broadcast::Sender<DaemonGoalEvent>,
}

impl Default for GoalEventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(32);
        Self { tx }
    }
}

impl GoalEventBus {
    /// Fresh bus; subscribers attach later via [`Self::subscribe`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to lifecycle events. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonGoalEvent> {
        self.tx.subscribe()
    }

    /// Publish one event. Returns silently when there are no subscribers —
    /// the steady state before any TUI is open.
    pub fn publish(&self, evt: DaemonGoalEvent) {
        let _ = self.tx.send(evt);
    }

    /// Build a [`cortex::goals::GoalEventSink`] handle that forwards
    /// emissions onto this bus. Hand this to `Agent::set_goal_event_sink`.
    pub fn as_sink(&self) -> Arc<dyn GoalEventSink> {
        Arc::new(BusSink { bus: self.clone() })
    }
}

/// Trait-object adapter the agent loop drives.
struct BusSink {
    bus: GoalEventBus,
}

impl GoalEventSink for BusSink {
    fn emit(&self, kind: &str, session_id: &str, goal: Option<&Goal>) {
        self.bus.publish(DaemonGoalEvent {
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            goal: goal.cloned(),
            emitted_at_unix: now_unix(),
        });
    }
}

/// Wall-clock seconds since the UNIX epoch. Saturates to 0 on a backward
/// system clock — `GoalEvent.emitted_at_unix` is informational, not a key.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::goals::event_kind;
    use cortex::goals::GoalStatus;

    fn sample_goal() -> Goal {
        Goal {
            session_id: "s".into(),
            goal_id: "g".into(),
            objective: "ship".into(),
            status: GoalStatus::Active,
            token_budget: Some(1000),
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let bus = GoalEventBus::new();
        let mut rx = bus.subscribe();
        let g = sample_goal();
        bus.as_sink().emit(event_kind::CREATED, "s", Some(&g));
        let evt = rx.recv().await.expect("event delivered");
        assert_eq!(evt.kind, "created");
        assert_eq!(evt.session_id, "s");
        assert_eq!(evt.goal.expect("goal present").goal_id, "g");
    }

    #[tokio::test]
    async fn cleared_event_carries_no_goal() {
        let bus = GoalEventBus::new();
        let mut rx = bus.subscribe();
        bus.as_sink().emit(event_kind::CLEARED, "s", None);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.kind, "cleared");
        assert!(evt.goal.is_none());
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_error() {
        let bus = GoalEventBus::new();
        // No `subscribe()` call here — `send` would return Err but the
        // wrapper swallows it. The assertion is "this does not panic and
        // does not require subscribers to function".
        bus.as_sink().emit("created", "s", Some(&sample_goal()));
    }
}
