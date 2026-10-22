use std::collections::VecDeque;
use std::time::{Duration, Instant};

use cortex::constants;
use sha2::{Digest, Sha256};

const MAX_ENTRIES: usize = constants::SELF_CHAT_CACHE_MAX_ENTRIES;
const TTL: Duration = constants::SELF_CHAT_CACHE_TTL;

struct Entry {
    text_hash: String,
    chat_identifier: String,
    created: Instant,
}

/// Tracks messages sent by the user themselves (is_from_me) to detect
/// self-chat reflection. When a message arrives from `is_from_me = true`,
/// we record it. When an inbound message arrives, we check if it matches
/// a recent self-sent message in the same chat — if so, it's the same
/// message echoed back.
pub struct SelfChatCache {
    entries: VecDeque<Entry>,
}

impl Default for SelfChatCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfChatCache {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// Record a self-sent message.
    pub fn remember(&mut self, text: &str, chat_identifier: &str) {
        self.prune();

        if self.entries.len() >= MAX_ENTRIES {
            self.entries.pop_front();
        }

        self.entries.push_back(Entry {
            text_hash: hash_text(text),
            chat_identifier: chat_identifier.to_string(),
            created: Instant::now(),
        });
    }

    /// Check if an inbound message matches a recent self-sent message.
    pub fn is_self_echo(&mut self, text: &str, chat_identifier: &str) -> bool {
        self.prune();
        let hash = hash_text(text);
        self.entries
            .iter()
            .any(|e| e.text_hash == hash && e.chat_identifier == chat_identifier)
    }

    fn prune(&mut self) {
        let now = Instant::now();
        while let Some(front) = self.entries.front() {
            if now.duration_since(front.created) >= TTL {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }
}

fn hash_text(text: &str) -> String {
    let normalized = text.trim().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_echo_detection() {
        let mut cache = SelfChatCache::new();
        cache.remember("hello", "iMessage;-;+1234567890");
        assert!(cache.is_self_echo("hello", "iMessage;-;+1234567890"));
        assert!(!cache.is_self_echo("hello", "iMessage;-;+9999999999"));
        assert!(!cache.is_self_echo("different", "iMessage;-;+1234567890"));
    }

    #[test]
    fn lru_eviction() {
        let mut cache = SelfChatCache::new();
        for i in 0..MAX_ENTRIES + 10 {
            cache.remember(&format!("msg-{i}"), "chat");
        }
        assert!(cache.entries.len() <= MAX_ENTRIES);
    }

    #[test]
    fn remember_keeps_all_entries_below_capacity() {
        // Eviction must only kick in AT capacity — below it, every
        // remembered message stays queryable. An inverted comparison would
        // evict on every insert and remember only the most recent message.
        let mut cache = SelfChatCache::new();
        cache.remember("first", "chat");
        cache.remember("second", "chat");
        assert!(cache.is_self_echo("first", "chat"));
        assert!(cache.is_self_echo("second", "chat"));
    }

    #[test]
    fn eviction_starts_exactly_at_capacity() {
        let mut cache = SelfChatCache::new();
        for i in 0..MAX_ENTRIES {
            cache.remember(&format!("msg-{i}"), "chat");
        }
        // At exactly MAX_ENTRIES nothing has been evicted yet.
        assert!(cache.is_self_echo("msg-0", "chat"));
        // One past capacity evicts only the oldest.
        cache.remember("overflow", "chat");
        assert!(!cache.is_self_echo("msg-0", "chat"));
        assert!(cache.is_self_echo("msg-1", "chat"));
        assert!(cache.is_self_echo("overflow", "chat"));
    }

    #[test]
    fn prune_expires_entries_past_ttl() {
        let mut cache = SelfChatCache::new();
        cache.remember("stale", "chat");
        // Backdate the entry past the TTL — Instant can't be mocked, so age
        // the stored timestamp directly (tests live in-module).
        let backdated = Instant::now()
            .checked_sub(TTL + Duration::from_secs(1))
            .expect("backdate instant");
        cache.entries.front_mut().expect("entry present").created = backdated;
        assert!(
            !cache.is_self_echo("stale", "chat"),
            "an entry older than the TTL must be pruned, not matched"
        );
        assert!(cache.entries.is_empty(), "prune must drop expired entries");
    }
}
