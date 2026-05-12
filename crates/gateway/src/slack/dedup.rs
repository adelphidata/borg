use cortex::constants;

use crate::message_processing::dedup::BoundedDedup;

/// Bounded deduplicator for Slack events.
///
/// Tracks two independent keyspaces:
/// - `event_id` (Slack's unique event identifier) — guards against webhook retries
/// - `(channel, ts)` — guards against Slack delivering the same underlying message as
///   both `message` and `app_mention` events (two different `event_id`s for one user action)
pub struct EventDeduplicator {
    by_event_id: BoundedDedup<String>,
    by_channel_ts: BoundedDedup<String>,
}

impl EventDeduplicator {
    pub fn new() -> Self {
        Self {
            by_event_id: BoundedDedup::new(constants::SLACK_DEDUP_CAPACITY),
            by_channel_ts: BoundedDedup::new(constants::SLACK_DEDUP_CAPACITY),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            by_event_id: BoundedDedup::new(capacity),
            by_channel_ts: BoundedDedup::new(capacity),
        }
    }

    /// Returns `true` if this `event_id` has been seen before.
    pub fn is_duplicate(&mut self, event_id: &str) -> bool {
        self.by_event_id.is_duplicate(&event_id.to_string())
    }

    /// Returns `true` if a prior event with the same `(channel, ts)` has been seen.
    ///
    /// Slack delivers `message` and `app_mention` as separate events with distinct
    /// `event_id`s for a single user @mention in a channel. Both share the same
    /// channel + message timestamp, so this key deduplicates them.
    pub fn is_duplicate_channel_ts(&mut self, channel: &str, ts: &str) -> bool {
        let key = format!("{channel}:{ts}");
        self.by_channel_ts.is_duplicate(&key)
    }
}

impl Default for EventDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_not_duplicate() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate("Ev001"));
    }

    #[test]
    fn second_seen_is_duplicate() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate("Ev001"));
        assert!(dedup.is_duplicate("Ev001"));
    }

    #[test]
    fn different_ids_not_duplicate() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate("Ev001"));
        assert!(!dedup.is_duplicate("Ev002"));
        assert!(!dedup.is_duplicate("Ev003"));
    }

    #[test]
    fn eviction_at_capacity() {
        let mut dedup = EventDeduplicator::with_capacity(3);

        assert!(!dedup.is_duplicate("Ev1"));
        assert!(!dedup.is_duplicate("Ev2"));
        assert!(!dedup.is_duplicate("Ev3"));
        // Capacity full — next insert evicts Ev1
        assert!(!dedup.is_duplicate("Ev4"));
        // Ev1 was evicted, no longer detected as duplicate
        assert!(!dedup.is_duplicate("Ev1"));
        // Ev2 was evicted by Ev1's re-insertion
        assert!(!dedup.is_duplicate("Ev2"));
    }

    #[test]
    fn duplicate_after_many_inserts() {
        let mut dedup = EventDeduplicator::new();
        for i in 1..=500 {
            assert!(!dedup.is_duplicate(&format!("Ev{i}")));
        }
        // All 500 should still be in the buffer (capacity=5000)
        assert!(dedup.is_duplicate("Ev1"));
        assert!(dedup.is_duplicate("Ev500"));
    }

    #[test]
    fn channel_ts_dedup_first_seen() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate_channel_ts("C123", "1234567890.111111"));
    }

    #[test]
    fn channel_ts_dedup_second_seen() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate_channel_ts("C123", "1234567890.111111"));
        assert!(dedup.is_duplicate_channel_ts("C123", "1234567890.111111"));
    }

    #[test]
    fn channel_ts_dedup_different_channel_same_ts() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate_channel_ts("C123", "1234567890.111111"));
        assert!(!dedup.is_duplicate_channel_ts("C456", "1234567890.111111"));
    }

    #[test]
    fn channel_ts_dedup_independent_of_event_id() {
        let mut dedup = EventDeduplicator::new();
        // Different event_ids, same channel+ts — second should be a duplicate
        // on the (channel, ts) axis even though event_ids differ.
        assert!(!dedup.is_duplicate("EvA"));
        assert!(!dedup.is_duplicate_channel_ts("C1", "111.222"));
        assert!(!dedup.is_duplicate("EvB"));
        assert!(dedup.is_duplicate_channel_ts("C1", "111.222"));
    }
}
