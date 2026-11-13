//! Meetings in the daemon: the event bus the `Meetings` service streams,
//! and the workers behind it — the summary writer (`finalizer`), the
//! calendar refresher (`calendar_sync`), the Google sign-in listener
//! (`oauth_loopback`) and, on macOS, the call detector (`detector`).
//!
//! The logic they drive lives in `cortex::meetings`; this module owns only
//! the loops, the timers and the bus.

pub mod calendar_sync;
pub mod detector;
pub mod finalizer;
pub mod oauth_loopback;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use protocols::meetings::{meeting_event::Kind, MeetingEvent};
use tokio::sync::{broadcast, mpsc, Notify};

/// Depth of the finalize queue. A full queue is harmless: the finalizer's
/// periodic scan picks up any meeting left `finalizing`.
const FINALIZE_QUEUE: usize = 32;

/// Broadcast of [`MeetingEvent`]s to every `SubscribeMeetingEvents` stream.
/// Capacity 64 absorbs a burst of transcript appends from both tracks while
/// a client reconnects; a lagging subscriber skips ahead and refetches.
#[derive(Clone)]
pub struct MeetingEventBus {
    tx: broadcast::Sender<MeetingEvent>,
}

impl Default for MeetingEventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }
}

impl MeetingEventBus {
    /// Listen for events. Drop the receiver to stop.
    pub fn subscribe(&self) -> broadcast::Receiver<MeetingEvent> {
        self.tx.subscribe()
    }

    /// Send one event. With nobody listening (no app open) it is dropped:
    /// every event is a hint to refetch, never the only copy of state.
    pub fn publish(&self, kind: Kind) {
        let _ = self.tx.send(MeetingEvent { kind: Some(kind) });
    }
}

/// What the `Meetings` service shares with the workers.
#[derive(Clone)]
pub struct MeetingsHandles {
    /// Events for `SubscribeMeetingEvents`.
    pub bus: MeetingEventBus,
    /// `StopMeeting` queues the meeting here for its summary.
    pub finalize_tx: mpsc::Sender<String>,
    /// Wakes the calendar worker early (after a connect or disconnect).
    pub calendar_poke: Arc<Notify>,
    /// The detector found it can run here (macOS 14+).
    pub detection_supported: Arc<AtomicBool>,
}

impl MeetingsHandles {
    /// Fresh handles and the finalize queue's receiving end, which the
    /// finalizer worker takes.
    pub fn new() -> (Self, mpsc::Receiver<String>) {
        let (finalize_tx, finalize_rx) = mpsc::channel(FINALIZE_QUEUE);
        (
            Self {
                bus: MeetingEventBus::default(),
                finalize_tx,
                calendar_poke: Arc::new(Notify::new()),
                detection_supported: Arc::new(AtomicBool::new(false)),
            },
            finalize_rx,
        )
    }

    /// Handles with no workers behind them, for tests and for a daemon
    /// whose orchestrator could not start. Stopped meetings still reach
    /// `finalizing`; a later daemon with workers summarises them.
    pub fn detached() -> Self {
        Self::new().0
    }
}
