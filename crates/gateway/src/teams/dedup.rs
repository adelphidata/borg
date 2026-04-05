use borg_core::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Microsoft Teams activity IDs.
    ///
    /// The Bot Framework retries webhook deliveries on 5xx responses and
    /// occasionally replays on timeouts. Dedup by activity ID so a retried
    /// message does not double-invoke the agent.
    pub struct ActivityDeduplicator(String, constants::TEAMS_DEDUP_CAPACITY);
    is_duplicate(activity_id: &str) => activity_id.to_string();
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_not_duplicate() {
        let mut dedup = ActivityDeduplicator::new();
        assert!(!dedup.is_duplicate("act-1"));
    }

    #[test]
    fn second_seen_is_duplicate() {
        let mut dedup = ActivityDeduplicator::new();
        assert!(!dedup.is_duplicate("act-1"));
        assert!(dedup.is_duplicate("act-1"));
    }

    #[test]
    fn different_ids_not_duplicate() {
        let mut dedup = ActivityDeduplicator::new();
        assert!(!dedup.is_duplicate("act-1"));
        assert!(!dedup.is_duplicate("act-2"));
        assert!(!dedup.is_duplicate("act-3"));
    }
}
