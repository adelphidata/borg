use cortex::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Telegram update IDs.
    pub struct UpdateDeduplicator(i64, constants::TELEGRAM_DEDUP_CAPACITY);
    is_duplicate(update_id: i64) => update_id;
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_not_duplicate() {
        let mut dedup = UpdateDeduplicator::new();
        assert!(!dedup.is_duplicate(1));
    }

    #[test]
    fn second_seen_is_duplicate() {
        let mut dedup = UpdateDeduplicator::new();
        assert!(!dedup.is_duplicate(1));
        assert!(dedup.is_duplicate(1));
    }

    #[test]
    fn different_ids_not_duplicate() {
        let mut dedup = UpdateDeduplicator::new();
        assert!(!dedup.is_duplicate(1));
        assert!(!dedup.is_duplicate(2));
        assert!(!dedup.is_duplicate(3));
    }

    #[test]
    fn eviction_at_capacity() {
        let mut dedup = UpdateDeduplicator::with_capacity(3);

        assert!(!dedup.is_duplicate(1));
        assert!(!dedup.is_duplicate(2));
        assert!(!dedup.is_duplicate(3));
        // Capacity full — next insert evicts 1
        assert!(!dedup.is_duplicate(4));
        // 1 was evicted, no longer detected as duplicate
        assert!(!dedup.is_duplicate(1));
        // After adding 4, order is [2,3,4]. Adding 1 evicts 2 -> [3,4,1]
        assert!(!dedup.is_duplicate(2));
    }

    #[test]
    fn duplicate_after_many_inserts() {
        let mut dedup = UpdateDeduplicator::new();
        for i in 1..=500 {
            assert!(!dedup.is_duplicate(i));
        }
        // All 500 should still be in the buffer (capacity=1000)
        assert!(dedup.is_duplicate(1));
        assert!(dedup.is_duplicate(500));
    }
}
