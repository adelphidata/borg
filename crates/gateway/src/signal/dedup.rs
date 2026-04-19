//! Deduplicator for Signal inbound messages, keyed on (sender_id, timestamp).
//!
//! Prevents duplicate processing when the SSE stream reconnects and replays
//! recent events. Wraps the shared [`crate::dedup::BoundedDedup`] so the LRU
//! eviction policy matches the other channels.

crate::dedup_wrapper!(
    /// Message deduplicator: (sender_id, timestamp) → seen.
    pub struct MessageDeduplicator((String, i64), borg_core::constants::SIGNAL_DEDUP_CAPACITY);
    is_duplicate(key: (&str, i64)) => (key.0.to_string(), key.1);
);

impl MessageDeduplicator {
    /// Convenience wrapper for the common call site `is_duplicate(sender, ts)`.
    pub fn seen(&mut self, sender_id: &str, timestamp: i64) -> bool {
        self.is_duplicate((sender_id, timestamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_message_not_duplicate() {
        let mut dedup = MessageDeduplicator::with_capacity(100);
        assert!(!dedup.seen("+15551234567", 1700000000000));
    }

    #[test]
    fn same_message_is_duplicate() {
        let mut dedup = MessageDeduplicator::with_capacity(100);
        assert!(!dedup.seen("+15551234567", 1700000000000));
        assert!(dedup.seen("+15551234567", 1700000000000));
    }

    #[test]
    fn different_sender_same_timestamp_not_duplicate() {
        let mut dedup = MessageDeduplicator::with_capacity(100);
        assert!(!dedup.seen("+15551234567", 1700000000000));
        assert!(!dedup.seen("+15559876543", 1700000000000));
    }

    #[test]
    fn same_sender_different_timestamp_not_duplicate() {
        let mut dedup = MessageDeduplicator::with_capacity(100);
        assert!(!dedup.seen("+15551234567", 1700000000000));
        assert!(!dedup.seen("+15551234567", 1700000001000));
    }

    #[test]
    fn eviction_on_capacity() {
        let mut dedup = MessageDeduplicator::with_capacity(3);
        assert!(!dedup.seen("a", 1));
        assert!(!dedup.seen("b", 2));
        assert!(!dedup.seen("c", 3));
        // Capacity full — next insert evicts oldest ("a", 1)
        assert!(!dedup.seen("d", 4));
        // "a" was evicted → treated as new again.
        assert!(!dedup.seen("a", 1));
    }
}
