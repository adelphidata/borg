//! Shared HMAC chain infrastructure for event-sourced ledgers.
//!
//! Provides a generic HMAC builder, chain verification, per-hour rate limiting,
//! and periodic checkpointing. Used by vitals, bond, and evolution systems.
//!
//! Fields are concatenated without separators to maintain backward compatibility
//! with existing event chains in user databases.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;

use crate::constants;

type HmacSha256 = Hmac<Sha256>;

/// Compute an HMAC-SHA256 over a sequence of fields.
///
/// Fields are concatenated directly (no separators) to maintain backward
/// compatibility with existing persisted event chains.
#[allow(clippy::expect_used)]
pub fn compute_hmac(key: &[u8], fields: &[&[u8]]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
    for field in fields {
        mac.update(field);
    }
    hex_encode(&mac.finalize().into_bytes())
}

/// Verify that an event's prev_hmac matches the expected chain value and
/// that the stored HMAC matches a recomputed value.
pub fn verify_chain_link(
    stored_hmac: &str,
    stored_prev_hmac: &str,
    expected_prev_hmac: &str,
    recomputed_hmac: &str,
) -> bool {
    stored_prev_hmac == expected_prev_hmac && stored_hmac == recomputed_hmac
}

/// Hex-encode a byte slice to a lowercase hex string.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Per-hour-bucket rate limiter for event replay.
///
/// Tracks counts of events by (hour_bucket, category_key) and enforces
/// per-category caps, optional total-per-hour cap, and optional positive-delta cap.
pub struct HourlyRateLimiter {
    type_counts: HashMap<(i64, String), u32>,
    total_counts: HashMap<i64, u32>,
    positive_counts: HashMap<i64, u32>,
    total_cap: Option<u32>,
    positive_cap: Option<u32>,
}

impl HourlyRateLimiter {
    /// Create a new rate limiter.
    ///
    /// - `total_cap`: if set, max total events per hour across all types
    /// - `positive_cap`: if set, max positive-delta events per hour
    pub fn new(total_cap: Option<u32>, positive_cap: Option<u32>) -> Self {
        Self {
            type_counts: HashMap::new(),
            total_counts: HashMap::new(),
            positive_counts: HashMap::new(),
            total_cap,
            positive_cap,
        }
    }

    /// Check if an event is within rate limits and consume a slot if so.
    ///
    /// Returns `true` if the event is allowed, `false` if rate-limited.
    pub fn check_and_consume(
        &mut self,
        timestamp: i64,
        event_type: &str,
        type_cap: u32,
        is_positive: bool,
    ) -> bool {
        let hour_bucket = timestamp / constants::SECS_PER_HOUR;

        // Total events per hour cap
        if let Some(cap) = self.total_cap {
            let total = self.total_counts.entry(hour_bucket).or_insert(0);
            if *total >= cap {
                return false;
            }
        }

        // Positive-delta events per hour cap
        if is_positive {
            if let Some(cap) = self.positive_cap {
                let pos = self.positive_counts.entry(hour_bucket).or_insert(0);
                if *pos >= cap {
                    return false;
                }
            }
        }

        // Per-type per hour cap
        let key = (hour_bucket, event_type.to_string());
        let count = self.type_counts.entry(key).or_insert(0);
        if *count >= type_cap {
            return false;
        }

        // All checks passed — consume slots
        *count += 1;
        if self.total_cap.is_some() {
            *self.total_counts.entry(hour_bucket).or_insert(0) += 1;
        }
        if is_positive && self.positive_cap.is_some() {
            *self.positive_counts.entry(hour_bucket).or_insert(0) += 1;
        }

        true
    }

    /// Source-specific rate limiting (separate from type-based).
    /// Returns `true` if allowed.
    pub fn check_source(&mut self, timestamp: i64, source: &str, cap: u32) -> bool {
        let hour_bucket = timestamp / constants::SECS_PER_HOUR;
        let key = (hour_bucket, format!("__src__{source}"));
        let count = self.type_counts.entry(key).or_insert(0);
        if *count >= cap {
            return false;
        }
        *count += 1;
        true
    }

    /// Source-specific diminishing returns. Returns a multiplier (0.0..=1.0)
    /// based on how many times this source has been seen this hour.
    /// Increments the internal counter on each call.
    pub fn source_decay_multiplier(&mut self, timestamp: i64, source: &str) -> f64 {
        let hour_bucket = timestamp / constants::SECS_PER_HOUR;
        let key = (hour_bucket, format!("__decay__{source}"));
        let count = self.type_counts.entry(key).or_insert(0);
        *count += 1;
        match *count {
            1..=2 => 1.0,
            3 => 0.5,
            4 => 0.25,
            _ => 0.0,
        }
    }
}

/// HMAC chain checkpoint for recovery after corruption.
#[derive(Debug, Clone)]
pub struct ChainCheckpoint {
    pub id: i64,
    pub domain: String,
    pub event_id: i64,
    pub prev_hmac: String,
    pub state_hash: String,
    pub created_at: i64,
}

/// How often to write checkpoints (every N verified events).
pub const CHECKPOINT_INTERVAL: u32 = constants::HMAC_CHECKPOINT_INTERVAL;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_hmac_deterministic() {
        let key = b"test-key";
        let h1 = compute_hmac(key, &[b"prev", b"category", b"source"]);
        let h2 = compute_hmac(key, &[b"prev", b"category", b"source"]);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn compute_hmac_different_fields() {
        let key = b"test-key";
        // Different field contents should produce different HMACs
        let h1 = compute_hmac(key, &[b"alpha", b"beta"]);
        let h2 = compute_hmac(key, &[b"alpha", b"gamma"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn compute_hmac_changes_with_key() {
        let h1 = compute_hmac(b"key-1", &[b"data"]);
        let h2 = compute_hmac(b"key-2", &[b"data"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn verify_chain_link_valid() {
        assert!(verify_chain_link("hmac1", "prev0", "prev0", "hmac1"));
    }

    #[test]
    fn verify_chain_link_bad_prev() {
        assert!(!verify_chain_link("hmac1", "wrong", "prev0", "hmac1"));
    }

    #[test]
    fn verify_chain_link_bad_hmac() {
        assert!(!verify_chain_link("wrong", "prev0", "prev0", "hmac1"));
    }

    #[test]
    fn rate_limiter_basic() {
        let mut rl = HourlyRateLimiter::new(None, None);
        for _ in 0..5 {
            assert!(rl.check_and_consume(3600, "test", 5, false));
        }
        assert!(!rl.check_and_consume(3600, "test", 5, false));
        // Different hour bucket should work
        assert!(rl.check_and_consume(7200, "test", 5, false));
    }

    #[test]
    fn rate_limiter_total_cap() {
        let mut rl = HourlyRateLimiter::new(Some(3), None);
        assert!(rl.check_and_consume(3600, "a", 10, false));
        assert!(rl.check_and_consume(3600, "b", 10, false));
        assert!(rl.check_and_consume(3600, "c", 10, false));
        assert!(!rl.check_and_consume(3600, "d", 10, false));
    }

    #[test]
    fn rate_limiter_positive_cap() {
        let mut rl = HourlyRateLimiter::new(None, Some(2));
        assert!(rl.check_and_consume(3600, "a", 10, true));
        assert!(rl.check_and_consume(3600, "b", 10, true));
        assert!(!rl.check_and_consume(3600, "c", 10, true));
        // Negative events bypass positive cap
        assert!(rl.check_and_consume(3600, "d", 10, false));
    }

    #[test]
    fn rate_limiter_source_cap() {
        let mut rl = HourlyRateLimiter::new(None, None);
        for _ in 0..3 {
            assert!(rl.check_source(3600, "run_shell", 3));
        }
        assert!(!rl.check_source(3600, "run_shell", 3));
        // Different source should work
        assert!(rl.check_source(3600, "other", 3));
    }

    #[test]
    fn source_decay_multiplier_schedule() {
        let mut rl = HourlyRateLimiter::new(None, None);
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 1.0); // count=1
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 1.0); // count=2
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.5); // count=3
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.25); // count=4
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.0); // count=5
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.0); // count=6
    }

    #[test]
    fn source_decay_per_source_isolation() {
        let mut rl = HourlyRateLimiter::new(None, None);
        // Exhaust run_shell
        for _ in 0..5 {
            rl.source_decay_multiplier(3600, "run_shell");
        }
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.0);
        // Different source starts fresh
        assert_eq!(rl.source_decay_multiplier(3600, "apply_patch"), 1.0);
    }

    #[test]
    fn source_decay_resets_each_hour() {
        let mut rl = HourlyRateLimiter::new(None, None);
        // Use up decay in hour 1
        for _ in 0..5 {
            rl.source_decay_multiplier(3600, "run_shell");
        }
        assert_eq!(rl.source_decay_multiplier(3600, "run_shell"), 0.0);
        // New hour resets
        assert_eq!(rl.source_decay_multiplier(7200, "run_shell"), 1.0);
    }

    #[test]
    fn hex_encode_correct() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x0a, 0x1b]), "00ff0a1b");
    }
}
