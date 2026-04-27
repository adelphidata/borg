//! Error deduplication store for gateway channels.
//!
//! Tracks recently sent error messages per channel+sender scope and suppresses
//! duplicates within a configurable cooldown window. This prevents error spam
//! when a provider is down for an extended period.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cortex::config::ErrorPolicy;

/// Maximum entries before pruning old ones.
const MAX_STORE_SIZE: usize = 100;

/// A scope key identifying a unique channel+sender+thread combination.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScopeKey {
    channel: String,
    sender_id: String,
    thread_id: String,
}

/// A tracked error entry with expiry time.
#[derive(Debug)]
struct ErrorEntry {
    /// The classified error category (not the full message).
    category: String,
    /// When this entry expires and errors can be shown again.
    expires_at: Instant,
}

/// Thread-safe error deduplication store.
#[derive(Debug)]
pub struct ErrorDedupStore {
    entries: HashMap<ScopeKey, Vec<ErrorEntry>>,
    default_cooldown: Duration,
}

impl ErrorDedupStore {
    /// Create a new store with the given default cooldown.
    pub fn new(cooldown_ms: u64) -> Self {
        Self {
            entries: HashMap::new(),
            default_cooldown: Duration::from_millis(cooldown_ms),
        }
    }

    /// Check whether an error should be suppressed for the given scope.
    ///
    /// If the error is **not** suppressed, it's recorded in the store so future
    /// duplicates within the cooldown window will be suppressed.
    ///
    /// Returns `true` if the error should be **suppressed** (not sent to user).
    pub fn should_suppress(
        &mut self,
        policy: ErrorPolicy,
        channel: &str,
        sender_id: &str,
        thread_id: Option<&str>,
        error_category: &str,
    ) -> bool {
        match policy {
            ErrorPolicy::Always => false,
            ErrorPolicy::Silent => true,
            ErrorPolicy::Once => {
                self.prune_if_needed();

                let key = ScopeKey {
                    channel: channel.to_string(),
                    sender_id: sender_id.to_string(),
                    thread_id: thread_id.unwrap_or("main").to_string(),
                };

                let now = Instant::now();

                // Check if we have a non-expired entry for this category
                if let Some(entries) = self.entries.get(&key) {
                    for entry in entries {
                        if entry.category == error_category && entry.expires_at > now {
                            return true; // suppress
                        }
                    }
                }

                // Not suppressed — record this error
                let entry = ErrorEntry {
                    category: error_category.to_string(),
                    expires_at: now + self.default_cooldown,
                };
                self.entries.entry(key).or_default().push(entry);
                false
            }
        }
    }

    /// Remove expired entries to prevent unbounded growth.
    fn prune_if_needed(&mut self) {
        let total: usize = self.entries.values().map(Vec::len).sum();
        if total <= MAX_STORE_SIZE {
            return;
        }

        let now = Instant::now();
        self.entries.retain(|_, entries| {
            entries.retain(|e| e.expires_at > now);
            !entries.is_empty()
        });
    }

    /// Clear all entries (e.g., for testing).
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for ErrorDedupStore {
    fn default() -> Self {
        Self::new(14_400_000) // 4 hours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(cooldown_ms: u64) -> ErrorDedupStore {
        ErrorDedupStore::new(cooldown_ms)
    }

    // ── ErrorPolicy::Always never suppresses ──

    #[test]
    fn always_policy_never_suppresses() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(
            ErrorPolicy::Always,
            "telegram",
            "user1",
            None,
            "rate_limit"
        ));
        // Even after recording, still doesn't suppress
        assert!(!store.should_suppress(
            ErrorPolicy::Always,
            "telegram",
            "user1",
            None,
            "rate_limit"
        ));
    }

    // ── ErrorPolicy::Silent always suppresses ──

    #[test]
    fn silent_policy_always_suppresses() {
        let mut store = make_store(60_000);
        assert!(store.should_suppress(
            ErrorPolicy::Silent,
            "telegram",
            "user1",
            None,
            "rate_limit"
        ));
    }

    // ── ErrorPolicy::Once suppresses duplicates ──

    #[test]
    fn once_policy_first_error_not_suppressed() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
    }

    #[test]
    fn once_policy_duplicate_suppressed() {
        let mut store = make_store(60_000);
        // First occurrence: not suppressed
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
        // Second occurrence: suppressed
        assert!(store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
    }

    #[test]
    fn once_policy_different_category_not_suppressed() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
        // Different error category: not suppressed
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "billing"));
    }

    #[test]
    fn once_policy_different_sender_not_suppressed() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
        // Different sender: not suppressed
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user2", None, "rate_limit"));
    }

    #[test]
    fn once_policy_different_channel_not_suppressed() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
        // Different channel: not suppressed
        assert!(!store.should_suppress(ErrorPolicy::Once, "slack", "user1", None, "rate_limit"));
    }

    #[test]
    fn once_policy_different_thread_not_suppressed() {
        let mut store = make_store(60_000);
        assert!(!store.should_suppress(
            ErrorPolicy::Once,
            "telegram",
            "user1",
            Some("thread-1"),
            "rate_limit"
        ));
        // Different thread: not suppressed
        assert!(!store.should_suppress(
            ErrorPolicy::Once,
            "telegram",
            "user1",
            Some("thread-2"),
            "rate_limit"
        ));
    }

    // ── TTL expiry ──

    #[test]
    fn once_policy_expired_entry_not_suppressed() {
        // Use a zero-ms cooldown so entries expire immediately
        let mut store = make_store(0);
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
        // Entry should have already expired (0ms cooldown)
        // Small race window but 0ms is effectively instant
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(!store.should_suppress(ErrorPolicy::Once, "telegram", "user1", None, "rate_limit"));
    }

    // ── Pruning ──

    #[test]
    fn prune_removes_expired_entries() {
        let mut store = make_store(10); // 10ms cooldown
                                        // Insert many entries
        for i in 0..150 {
            store
                .entries
                .entry(ScopeKey {
                    channel: "telegram".to_string(),
                    sender_id: format!("user{i}"),
                    thread_id: "main".to_string(),
                })
                .or_default()
                .push(ErrorEntry {
                    category: "rate_limit".to_string(),
                    expires_at: Instant::now(), // already expired
                });
        }
        let total_before: usize = store.entries.values().map(|v| v.len()).sum();
        assert_eq!(total_before, 150);
        std::thread::sleep(std::time::Duration::from_millis(1));
        // Trigger prune
        store.prune_if_needed();
        let total_after: usize = store.entries.values().map(|v| v.len()).sum();
        assert_eq!(total_after, 0);
    }

    #[test]
    fn prune_keeps_active_entries() {
        let mut store = make_store(60_000); // 60s cooldown
        for i in 0..150 {
            store.should_suppress(
                ErrorPolicy::Once,
                "telegram",
                &format!("user{i}"),
                None,
                "rate_limit",
            );
        }
        // Trigger prune — entries are still active
        store.prune_if_needed();
        let total: usize = store.entries.values().map(|v| v.len()).sum();
        // Active entries should survive pruning
        assert!(total > 0);
    }

    // ── Default ──

    #[test]
    fn default_store_has_4_hour_cooldown() {
        let store = ErrorDedupStore::default();
        assert_eq!(store.default_cooldown, Duration::from_millis(14_400_000));
    }

    // ── ErrorPolicy display/parse ──

    #[test]
    fn error_policy_display() {
        assert_eq!(ErrorPolicy::Always.to_string(), "always");
        assert_eq!(ErrorPolicy::Once.to_string(), "once");
        assert_eq!(ErrorPolicy::Silent.to_string(), "silent");
    }

    #[test]
    fn error_policy_parse() {
        use std::str::FromStr;
        assert_eq!(
            ErrorPolicy::from_str("always").unwrap(),
            ErrorPolicy::Always
        );
        assert_eq!(ErrorPolicy::from_str("once").unwrap(), ErrorPolicy::Once);
        assert_eq!(
            ErrorPolicy::from_str("silent").unwrap(),
            ErrorPolicy::Silent
        );
        assert_eq!(ErrorPolicy::from_str("ONCE").unwrap(), ErrorPolicy::Once);
        assert!(ErrorPolicy::from_str("invalid").is_err());
    }

    // ── Config integration ──

    #[test]
    fn error_policy_default_is_once() {
        assert_eq!(ErrorPolicy::default(), ErrorPolicy::Once);
    }
}
