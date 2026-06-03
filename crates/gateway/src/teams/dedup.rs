use cortex::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Microsoft Teams activity IDs.
    ///
    /// The Bot Framework retries webhook deliveries on 5xx responses and
    /// occasionally replays on timeouts. Dedup by activity ID so a retried
    /// message does not double-invoke the agent.
    pub struct ActivityDeduplicator(String, constants::TEAMS_DEDUP_CAPACITY);
    is_duplicate(activity_id: &str) => activity_id.to_string();
);
