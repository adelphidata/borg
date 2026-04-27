//! Bounded deduplicator for Discord interaction IDs.
//!
//! Discord retries interaction webhook deliveries on non-2xx responses and
//! occasional transient network conditions. Without deduplication the agent
//! can be invoked twice for the same interaction, wasting tokens and producing
//! duplicate replies. Interaction IDs are Discord snowflakes and globally
//! unique, so they make an ideal dedup key.

use cortex::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Discord interaction IDs.
    pub struct InteractionDeduplicator(String, constants::DISCORD_DEDUP_CAPACITY);
    is_duplicate(interaction_id: &str) => interaction_id.to_string();
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_not_duplicate() {
        let mut dedup = InteractionDeduplicator::new();
        assert!(!dedup.is_duplicate("1234567890"));
    }

    #[test]
    fn second_seen_is_duplicate() {
        let mut dedup = InteractionDeduplicator::new();
        assert!(!dedup.is_duplicate("1234567890"));
        assert!(dedup.is_duplicate("1234567890"));
    }

    #[test]
    fn different_ids_not_duplicate() {
        let mut dedup = InteractionDeduplicator::new();
        assert!(!dedup.is_duplicate("1111"));
        assert!(!dedup.is_duplicate("2222"));
        assert!(!dedup.is_duplicate("3333"));
    }

    #[test]
    fn eviction_at_capacity() {
        let mut dedup = InteractionDeduplicator::with_capacity(3);

        assert!(!dedup.is_duplicate("a"));
        assert!(!dedup.is_duplicate("b"));
        assert!(!dedup.is_duplicate("c"));
        // Capacity full — next insert evicts "a"
        assert!(!dedup.is_duplicate("d"));
        // "a" was evicted, no longer detected as duplicate
        assert!(!dedup.is_duplicate("a"));
    }

    #[test]
    fn default_is_empty() {
        let mut dedup = InteractionDeduplicator::default();
        assert!(!dedup.is_duplicate("fresh"));
    }

    #[test]
    fn many_distinct_ids_retained() {
        let mut dedup = InteractionDeduplicator::new();
        for i in 0..500 {
            assert!(!dedup.is_duplicate(&format!("int{i}")));
        }
        // All still in the buffer (capacity = 5000)
        assert!(dedup.is_duplicate("int0"));
        assert!(dedup.is_duplicate("int499"));
    }
}
