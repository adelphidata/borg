use std::collections::{HashSet, VecDeque};

use sha2::{Digest, Sha256};

use cortex::constants;

const DEFAULT_CAPACITY: usize = constants::SLACK_ECHO_CACHE_CAPACITY;

/// Tracks SHA256 hashes of recently sent outbound messages to detect echoes.
/// Prevents the bot from responding to its own messages in edge cases where
/// the `bot_id` field is missing (e.g., `as_user: true`, unfurling).
pub struct EchoCache {
    order: VecDeque<[u8; 32]>,
    set: HashSet<[u8; 32]>,
    capacity: usize,
}

impl EchoCache {
    pub fn new() -> Self {
        Self {
            order: VecDeque::with_capacity(DEFAULT_CAPACITY),
            set: HashSet::with_capacity(DEFAULT_CAPACITY),
            capacity: DEFAULT_CAPACITY,
        }
    }

    fn hash(text: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.finalize().into()
    }

    /// Record an outbound message so it can be detected as an echo later.
    pub fn remember(&mut self, text: &str) {
        let h = Self::hash(text);
        if self.set.contains(&h) {
            return;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        self.order.push_back(h);
        self.set.insert(h);
    }

    /// Returns `true` if this text matches a recently sent outbound message.
    pub fn is_echo(&self, text: &str) -> bool {
        self.set.contains(&Self::hash(text))
    }
}

impl Default for EchoCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_echo_when_empty() {
        let cache = EchoCache::new();
        assert!(!cache.is_echo("hello"));
    }

    #[test]
    fn detects_echo_after_remember() {
        let mut cache = EchoCache::new();
        cache.remember("hello world");
        assert!(cache.is_echo("hello world"));
    }

    #[test]
    fn different_text_not_echo() {
        let mut cache = EchoCache::new();
        cache.remember("hello");
        assert!(!cache.is_echo("goodbye"));
    }

    #[test]
    fn eviction_at_capacity() {
        let mut cache = EchoCache {
            order: VecDeque::with_capacity(2),
            set: HashSet::with_capacity(2),
            capacity: 2,
        };

        cache.remember("a");
        cache.remember("b");
        assert!(cache.is_echo("a"));
        assert!(cache.is_echo("b"));

        // Adding third evicts "a"
        cache.remember("c");
        assert!(!cache.is_echo("a"));
        assert!(cache.is_echo("b"));
        assert!(cache.is_echo("c"));
    }

    #[test]
    fn duplicate_remember_is_noop() {
        let mut cache = EchoCache::new();
        cache.remember("hello");
        cache.remember("hello");
        assert_eq!(cache.order.len(), 1);
    }
}
