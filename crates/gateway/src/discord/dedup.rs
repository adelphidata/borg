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

// First-seen / repeat-seen / LRU eviction is exercised on the underlying
// `BoundedDedup<K>` in `message_processing::dedup`. The macro substitution
// here is a straight `&str -> String` passthrough, so per-channel re-tests
// would be near-duplicate fluff per CLAUDE.md.
