//! Broadcast bus for subscription activity, read by
//! `Admin.SubscribeSubscriptionEvents`.
//!
//! UI notifications only. The ledger table is the source of truth, so a
//! lagged or absent subscriber loses nothing a refetch cannot recover; that
//! is why publishing to no one is not an error.

use tokio::sync::broadcast;

/// What happened.
pub mod kind {
    /// An authenticated request reached the subscription (stored or not).
    pub const RECEIVED: &str = "received";
    /// A run started.
    pub const STARTED: &str = "started";
    /// A run finished; `status` says how.
    pub const FINISHED: &str = "finished";
    /// The subscription paused itself.
    pub const STORM: &str = "storm";
}

/// One activity event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionActivity {
    /// The subscription.
    pub subscription_id: String,
    /// One of [`kind`].
    pub kind: String,
    /// The run, for `started` and `finished`.
    pub run_id: Option<String>,
    /// The run's final status, for `finished`.
    pub status: Option<String>,
    /// One line for a toast or the status line.
    pub summary: String,
    /// Events handled by the run.
    pub event_count: u32,
    /// Unix seconds.
    pub emitted_at_unix: i64,
}

/// Cheap-to-clone bus.
#[derive(Clone)]
pub struct SubscriptionBus {
    tx: broadcast::Sender<SubscriptionActivity>,
}

impl Default for SubscriptionBus {
    fn default() -> Self {
        // Deeper than the goal bus: a burst produces one `received` per
        // drain, plus a start and a finish per run.
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }
}

impl SubscriptionBus {
    /// Fresh bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe; drop the receiver to stop.
    pub fn subscribe(&self) -> broadcast::Receiver<SubscriptionActivity> {
        self.tx.subscribe()
    }

    /// Publish one event. No subscriber is the steady state, not an error.
    pub fn publish(&self, activity: SubscriptionActivity) {
        let _ = self.tx.send(activity);
    }
}
