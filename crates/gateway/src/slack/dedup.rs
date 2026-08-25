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

    /// Slack's `EventDeduplicator` differs from the other channels by holding
    /// **two independent keyspaces**: `event_id` and `(channel, ts)`. Only the
    /// slack-specific invariant — that the two axes do not cross-pollute — is
    /// worth testing here; generic first-seen / LRU behavior is covered by
    /// `BoundedDedup` tests upstream.
    #[test]
    fn event_id_and_channel_ts_keyspaces_are_independent() {
        let mut dedup = EventDeduplicator::new();

        // Same (channel, ts) twice should dedupe even when interleaved with
        // unrelated `event_id` traffic — Slack delivers `message` and
        // `app_mention` as distinct events for one user action.
        assert!(!dedup.is_duplicate("EvA"));
        assert!(!dedup.is_duplicate_channel_ts("C1", "111.222"));
        assert!(!dedup.is_duplicate("EvB"));
        assert!(
            dedup.is_duplicate("EvA"),
            "a repeated event_id must dedupe — Slack retries redeliver the same event_id"
        );
        assert!(
            dedup.is_duplicate_channel_ts("C1", "111.222"),
            "(channel, ts) must dedupe regardless of event_id traffic"
        );

        // And the inverse: `event_id` keyspace must not mark a fresh
        // (channel, ts) tuple as seen just because the same string appeared
        // as an event_id.
        let mut dedup2 = EventDeduplicator::new();
        assert!(!dedup2.is_duplicate("C9:999.000"));
        assert!(
            !dedup2.is_duplicate_channel_ts("C9", "999.000"),
            "event_id traffic must not poison the (channel, ts) axis"
        );
    }

    /// Both `(channel, ts)` components must participate in the key. A bug
    /// that hashed only the channel (or only the timestamp) would silently
    /// drop legitimate messages.
    #[test]
    fn channel_ts_uses_both_components() {
        let mut dedup = EventDeduplicator::new();
        assert!(!dedup.is_duplicate_channel_ts("C123", "1234567890.111111"));
        assert!(
            !dedup.is_duplicate_channel_ts("C456", "1234567890.111111"),
            "different channel with same ts must not dedupe"
        );
        assert!(
            !dedup.is_duplicate_channel_ts("C123", "1234567890.222222"),
            "same channel with different ts must not dedupe"
        );
        assert!(
            dedup.is_duplicate_channel_ts("C123", "1234567890.111111"),
            "exact tuple repeat must dedupe"
        );
    }
}
