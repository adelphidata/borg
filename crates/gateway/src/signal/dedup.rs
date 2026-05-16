//! Deduplicator for Signal inbound messages, keyed on (sender_id, timestamp).
//!
//! Prevents duplicate processing when the SSE stream reconnects and replays
//! recent events. Wraps the shared [`crate::message_processing::dedup::BoundedDedup`] so the LRU
//! eviction policy matches the other channels.

crate::dedup_wrapper!(
    /// Message deduplicator: (sender_id, timestamp) → seen.
    pub struct MessageDeduplicator((String, i64), cortex::constants::SIGNAL_DEDUP_CAPACITY);
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

    /// Signal's dedup key is the tuple `(sender, timestamp)` — not just one
    /// component. Verifies the macro key extraction uses both: messages that
    /// share only one component must still be admitted as new. (Generic LRU
    /// behavior is covered by `BoundedDedup` tests; only the tuple-key axis
    /// is signal-specific.)
    #[test]
    fn tuple_key_uses_both_sender_and_timestamp() {
        let mut dedup = MessageDeduplicator::with_capacity(100);
        let sender_a = "+15551234567";
        let sender_b = "+15559876543";
        let ts1 = 1_700_000_000_000_i64;
        let ts2 = 1_700_000_001_000_i64;

        assert!(!dedup.seen(sender_a, ts1), "first sighting must be fresh");
        assert!(
            dedup.seen(sender_a, ts1),
            "exact (sender, ts) repeat must dedupe"
        );
        assert!(
            !dedup.seen(sender_b, ts1),
            "same ts, different sender must NOT dedupe \
             (timestamp collisions across senders are expected on Signal)"
        );
        assert!(
            !dedup.seen(sender_a, ts2),
            "same sender, different ts must NOT dedupe \
             (a new message from the same person)"
        );
    }
}
