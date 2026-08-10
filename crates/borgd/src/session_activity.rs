//! Broadcast bus for per-session turn activity.
//!
//! A session list wants to show which conversation is working right now. The
//! alternative to this bus is polling every session's host, which costs a
//! round trip per row per tick and still lags. Instead every
//! [`SessionHost`](crate::session::SessionHost) publishes the two transitions
//! that matter — a turn started, a turn settled — and
//! `Admin.SubscribeSessionActivity` fans them out.
//!
//! The feed carries deltas only. It is deliberately NOT a source of truth: a
//! subscriber that misses events (dropped stream, `Lagged` receiver) recovers
//! by reading `SessionSummary.turn_active` from a fresh `Admin.ListSessions`,
//! which reads the live registry. Design mirrors [`crate::goal_bus`]: a
//! cloneable wrapper over `tokio::sync::broadcast::Sender` whose `publish` is
//! fire-and-forget, because a UI notification with no listener is not an error.

use tokio::sync::broadcast;

/// One transition of a session between "a turn is running" and "settled".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActivity {
    /// Session whose turn state changed.
    pub session_id: String,
    /// `true` when a turn just started, `false` when it finished streaming.
    pub turn_active: bool,
    /// Server-side emit time (unix seconds), informational only.
    pub emitted_at_unix: i64,
}

/// Broadcast bus. Cheap to clone. Capacity 32 matches [`crate::goal_bus`] and
/// absorbs a burst of sessions starting turns together (a heartbeat fan-out)
/// without marking an attentive subscriber `Lagged`.
#[derive(Clone)]
pub struct SessionActivityBus {
    tx: broadcast::Sender<SessionActivity>,
}

impl Default for SessionActivityBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(32);
        Self { tx }
    }
}

impl SessionActivityBus {
    /// Fresh bus; subscribers attach later via [`Self::subscribe`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to transitions. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionActivity> {
        self.tx.subscribe()
    }

    /// Publish one transition. Silent when nothing is subscribed — the steady
    /// state whenever no session list is open.
    pub fn publish(&self, evt: SessionActivity) {
        let _ = self.tx.send(evt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_bus::now_unix;

    fn evt(id: &str, active: bool) -> SessionActivity {
        SessionActivity {
            session_id: id.to_string(),
            turn_active: active,
            emitted_at_unix: now_unix(),
        }
    }

    #[tokio::test]
    async fn subscribers_receive_both_transitions_in_order() {
        let bus = SessionActivityBus::new();
        let mut rx = bus.subscribe();
        bus.publish(evt("s", true));
        bus.publish(evt("s", false));
        let first = rx.recv().await.expect("start delivered");
        let second = rx.recv().await.expect("settle delivered");
        assert_eq!(first.session_id, "s");
        assert!(first.turn_active, "the start must arrive first");
        assert!(!second.turn_active, "the settle must arrive second");
    }

    #[tokio::test]
    async fn a_late_subscriber_misses_earlier_transitions() {
        // Documents why `ListSessions.turn_active` is the reconcile path: the
        // bus keeps no history, so a client that attaches mid-turn learns
        // nothing until the next transition.
        let bus = SessionActivityBus::new();
        bus.publish(evt("s", true));
        let mut rx = bus.subscribe();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "a subscriber attached after the start must not replay it"
        );
    }

    #[tokio::test]
    async fn a_bus_that_lost_every_subscriber_still_serves_the_next_one() {
        // Turns run whether or not a UI is attached, so publishing into an
        // empty channel must not poison the sender for the client that
        // connects afterwards.
        let bus = SessionActivityBus::new();
        bus.publish(evt("s", true));
        drop(bus.subscribe());
        bus.publish(evt("s", false));

        let mut rx = bus.subscribe();
        bus.publish(evt("s", true));
        let received = rx.recv().await.expect("a later subscriber still receives");
        assert_eq!(received.session_id, "s");
        assert!(received.turn_active);
    }
}
