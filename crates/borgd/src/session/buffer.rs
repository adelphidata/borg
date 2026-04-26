//! Per-session sequenced event ring + broadcast channel.
//!
//! Architecture: every emitted proto `AgentEvent` gets a monotonic `event_seq`,
//! is appended to a bounded `VecDeque` (oldest evicted at capacity), and is
//! broadcast to all live subscribers. New subscribers replay missed events from
//! the buffer using `since_event_seq`; lagged subscribers (broadcast capacity
//! exceeded) recover via the same buffer rather than dropping events silently.

use borg_proto::session::AgentEvent as ProtoEvent;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::broadcast;

/// Ring buffer capacity per session. 1k events is enough for any realistic
/// resume window — a single turn rarely exceeds a few hundred deltas.
const BUFFER_CAPACITY: usize = 1024;

/// Broadcast channel capacity. Receivers that fall this far behind get a
/// `Lagged` error, which the consumer translates into a buffer replay.
const BROADCAST_CAPACITY: usize = 256;

/// Append-and-fan-out store for proto events on a single session.
pub struct SequencedBuffer {
    /// Most-recent-first event_seq counter. `0` means "no events emitted yet".
    next_seq: Mutex<u64>,
    /// Bounded history of recent events for replay/resume.
    ring: Mutex<VecDeque<ProtoEvent>>,
    /// Live broadcast for subscribers that joined before `push`.
    bcast: broadcast::Sender<ProtoEvent>,
}

impl SequencedBuffer {
    /// Build a fresh buffer with sequence 0 and no events.
    pub fn new() -> Self {
        let (bcast, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            next_seq: Mutex::new(1),
            ring: Mutex::new(VecDeque::with_capacity(BUFFER_CAPACITY)),
            bcast,
        }
    }

    /// Stamp `event` with the next sequence number, append to the ring, and
    /// broadcast. Returns the assigned `event_seq`.
    pub fn push(&self, mut event: ProtoEvent) -> u64 {
        let seq = {
            let mut g = self
                .next_seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let s = *g;
            *g += 1;
            s
        };
        event.event_seq = seq;
        {
            let mut ring = self
                .ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if ring.len() == BUFFER_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        // send() returns Err only when there are no live receivers — that's
        // not a failure mode, it just means nobody's listening yet.
        let _ = self.bcast.send(event);
        seq
    }

    /// Snapshot every buffered event with `event_seq > since`. Cheap clone —
    /// caller flushes these into its outbound stream before subscribing.
    pub fn replay_since(&self, since: u64) -> Vec<ProtoEvent> {
        let ring = self
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ring.iter()
            .filter(|e| e.event_seq > since)
            .cloned()
            .collect()
    }

    /// Subscribe to new events from this point forward.
    pub fn subscribe(&self) -> broadcast::Receiver<ProtoEvent> {
        self.bcast.subscribe()
    }

    /// Highest assigned `event_seq`, or 0 if no events have been pushed.
    pub fn last_seq(&self) -> u64 {
        let g = self
            .next_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.saturating_sub(1)
    }
}

impl Default for SequencedBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borg_proto::session::{agent_event::Kind, TextDelta};

    fn delta(s: &str) -> ProtoEvent {
        ProtoEvent {
            event_seq: 0,
            kind: Some(Kind::TextDelta(TextDelta { text: s.into() })),
        }
    }

    #[test]
    fn push_assigns_monotonic_sequence_starting_at_one() {
        // Real failure mode: a regression to 0-indexed seqs would collide with
        // the protocol convention that `since=0` means "from the start".
        let buf = SequencedBuffer::new();
        assert_eq!(buf.push(delta("a")), 1);
        assert_eq!(buf.push(delta("b")), 2);
        assert_eq!(buf.last_seq(), 2);
    }

    #[test]
    fn replay_since_returns_only_events_after_cursor() {
        // Real failure mode: an off-by-one on the `>` would either replay the
        // already-seen event (duplicate) or skip the next one (gap).
        let buf = SequencedBuffer::new();
        for c in ["a", "b", "c", "d"] {
            buf.push(delta(c));
        }
        let got: Vec<_> = buf
            .replay_since(2)
            .into_iter()
            .filter_map(|e| match e.kind {
                Some(Kind::TextDelta(d)) => Some(d.text),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec!["c".to_string(), "d".to_string()]);
    }

    #[tokio::test]
    async fn broadcast_subscribers_see_pushes_after_subscribe() {
        let buf = SequencedBuffer::new();
        let mut rx = buf.subscribe();
        buf.push(delta("hello"));
        let evt = rx.recv().await.expect("recv ok");
        assert_eq!(evt.event_seq, 1);
        assert!(matches!(evt.kind, Some(Kind::TextDelta(_))));
    }

    #[test]
    fn ring_evicts_oldest_when_capacity_reached() {
        // Real failure mode: an unbounded VecDeque would let a long-running
        // session OOM the daemon. Capacity is 1024; we don't write 1k+ events
        // here for speed — just assert that at capacity the oldest is gone
        // by directly checking len <= cap and that push past cap evicts.
        let buf = SequencedBuffer::new();
        for _ in 0..(BUFFER_CAPACITY + 5) {
            buf.push(delta("x"));
        }
        let snap = buf.replay_since(0);
        assert_eq!(snap.len(), BUFFER_CAPACITY);
        // Earliest retained event_seq must equal total_pushed - capacity + 1.
        let earliest = snap.first().expect("non-empty").event_seq;
        assert_eq!(earliest, 6);
    }
}
