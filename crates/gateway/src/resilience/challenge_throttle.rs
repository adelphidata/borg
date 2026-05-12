//! Pairing challenge throttle for gateway channels.
//!
//! Prevents spamming unapproved senders with repeated pairing challenge messages.
//! Only the first challenge within a cooldown window is delivered; subsequent
//! messages from the same sender are silently suppressed until the cooldown expires.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default cooldown: 5 minutes between challenge messages per sender.
const DEFAULT_COOLDOWN_SECS: u64 = 300;

/// Maximum entries before pruning expired ones.
const MAX_ENTRIES: usize = 200;

/// Scope key identifying a unique channel + sender combination.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScopeKey {
    channel: String,
    sender_id: String,
}

/// Throttle store for pairing challenge messages.
#[derive(Debug)]
pub struct ChallengeThrottle {
    entries: HashMap<ScopeKey, Instant>,
    cooldown: Duration,
}

impl ChallengeThrottle {
    /// Create a new throttle with the given cooldown duration.
    pub fn new(cooldown: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            cooldown,
        }
    }

    /// Check whether a challenge for this sender should be suppressed.
    ///
    /// Returns `true` if a challenge was already sent within the cooldown
    /// window (i.e., the new challenge should be suppressed).
    ///
    /// If not suppressed, records the current timestamp so future calls
    /// within the cooldown will be suppressed.
    pub fn should_suppress(&mut self, channel: &str, sender_id: &str) -> bool {
        self.prune_if_needed();

        let key = ScopeKey {
            channel: channel.to_string(),
            sender_id: sender_id.to_string(),
        };

        let now = Instant::now();

        if let Some(last_sent) = self.entries.get(&key) {
            if now.duration_since(*last_sent) < self.cooldown {
                return true; // suppress
            }
        }

        // Not suppressed — record this challenge
        self.entries.insert(key, now);
        false
    }

    /// Remove expired entries to prevent unbounded growth.
    fn prune_if_needed(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }

        let now = Instant::now();
        self.entries
            .retain(|_, sent_at| now.duration_since(*sent_at) < self.cooldown);
    }
}

impl Default for ChallengeThrottle {
    fn default() -> Self {
        Self::new(Duration::from_secs(DEFAULT_COOLDOWN_SECS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_throttle(cooldown_ms: u64) -> ChallengeThrottle {
        ChallengeThrottle::new(Duration::from_millis(cooldown_ms))
    }

    #[test]
    fn scope_and_cooldown() {
        // Covers the four real branches of should_suppress:
        //   (a) first hit → not suppressed
        //   (b) repeat within cooldown → suppressed
        //   (c) same channel, different sender → independent
        //   (d) same sender, different channel → independent
        let mut throttle = make_throttle(60_000);
        assert!(!throttle.should_suppress("telegram", "user1"), "first hit");
        assert!(
            throttle.should_suppress("telegram", "user1"),
            "repeat within cooldown"
        );
        assert!(
            !throttle.should_suppress("telegram", "user2"),
            "per-sender isolation"
        );
        assert!(
            !throttle.should_suppress("slack", "user1"),
            "per-channel isolation"
        );
    }

    #[test]
    fn zero_cooldown_never_suppresses() {
        // Edge: a 0ms cooldown should mean "never suppress" — repeated hits
        // after a tick still fire through.
        let mut throttle = make_throttle(0);
        assert!(!throttle.should_suppress("telegram", "user1"));
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(!throttle.should_suppress("telegram", "user1"));
    }

    #[test]
    fn prune_removes_expired_entries() {
        let mut throttle = make_throttle(10); // 10ms cooldown
        for i in 0..250 {
            throttle.entries.insert(
                ScopeKey {
                    channel: "telegram".to_string(),
                    sender_id: format!("user{i}"),
                },
                Instant::now() - Duration::from_secs(60), // already expired
            );
        }
        assert_eq!(throttle.entries.len(), 250);
        // Trigger prune via should_suppress
        throttle.should_suppress("telegram", "new_user");
        // Expired entries should be removed, only the new one remains
        assert!(throttle.entries.len() <= 2);
    }

    #[test]
    fn prune_keeps_active_entries() {
        let mut throttle = make_throttle(60_000); // 60s cooldown
        for i in 0..250 {
            throttle.should_suppress("telegram", &format!("user{i}"));
        }
        // Trigger prune
        throttle.prune_if_needed();
        // Active entries should survive
        assert!(!throttle.entries.is_empty());
    }
}
