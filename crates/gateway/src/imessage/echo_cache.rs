use std::collections::HashMap;
use std::time::{Duration, Instant};

use cortex::constants;
use sha2::{Digest, Sha256};

/// Cache of recently sent messages to detect echoes.
///
/// Two lookup strategies:
/// - Text hash (SHA256 prefix): 5-second TTL (short to avoid blocking common replies)
/// - Message ROWID: 60-second TTL (stronger signal for exact match)
pub struct EchoCache {
    text_entries: HashMap<String, Instant>,
    id_entries: HashMap<i64, Instant>,
    text_ttl: Duration,
    id_ttl: Duration,
}

impl Default for EchoCache {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoCache {
    pub fn new() -> Self {
        Self {
            text_entries: HashMap::new(),
            id_entries: HashMap::new(),
            text_ttl: constants::ECHO_CACHE_TEXT_TTL,
            id_ttl: constants::ECHO_CACHE_ID_TTL,
        }
    }

    /// Record a sent message so future inbound copies are detected as echoes.
    pub fn remember(&mut self, text: &str, message_id: Option<i64>) {
        let hash = text_hash(text);
        self.text_entries.insert(hash, Instant::now());
        if let Some(id) = message_id {
            self.id_entries.insert(id, Instant::now());
        }
    }

    /// Check if an inbound message is an echo of something we recently sent.
    pub fn is_echo(&mut self, text: &str, message_id: Option<i64>) -> bool {
        self.prune();

        // Check by message ID first (strongest signal)
        if let Some(id) = message_id {
            if self.id_entries.contains_key(&id) {
                return true;
            }
        }

        // Check by text hash
        let hash = text_hash(text);
        self.text_entries.contains_key(&hash)
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.text_entries
            .retain(|_, ts| now.duration_since(*ts) < self.text_ttl);
        self.id_entries
            .retain(|_, ts| now.duration_since(*ts) < self.id_ttl);
    }
}

fn text_hash(text: &str) -> String {
    let normalized = text.trim().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let result = hasher.finalize();
    // Use first 16 hex chars as prefix
    format!("{result:x}")[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_detect_text_echo() {
        let mut cache = EchoCache::new();
        cache.remember("Hello world", None);
        assert!(cache.is_echo("Hello world", None));
        assert!(cache.is_echo("  hello world  ", None)); // normalized
        assert!(!cache.is_echo("Something else", None));
    }

    #[test]
    fn remember_and_detect_id_echo() {
        let mut cache = EchoCache::new();
        cache.remember("test", Some(42));
        assert!(cache.is_echo("different text", Some(42)));
        assert!(!cache.is_echo("different text", Some(99)));
    }

    #[test]
    fn non_echo_message_passes() {
        let mut cache = EchoCache::new();
        cache.remember("sent message", None);
        assert!(!cache.is_echo("new inbound", None));
    }
}
