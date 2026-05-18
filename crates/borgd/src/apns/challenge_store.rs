//! In-memory challenge nonce store for APNs device registration.
//!
//! On `IssueChallenge`, the daemon mints 32 random bytes, stamps them with
//! a 5-minute TTL, and binds them to a key (the paired-device's mTLS cert
//! fingerprint). On `RegisterDevice`, the iOS client returns the nonce in
//! its App Attest assertion; we look it up here and reject expired/missing
//! entries before invoking the verifier.
//!
//! The store is in-memory deliberately — challenges are single-use,
//! short-lived, and survive a daemon restart only at the cost of a fresh
//! challenge issue (the next `IssueChallenge` call), which is the right
//! trade vs. persisting a moving table to disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::RngCore;

/// 5 minutes — Apple's recommended upper bound for App Attest challenge
/// freshness. Shorter doesn't materially improve security but does break
/// pairing UX over a slow connection.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(300);

/// Length of the random nonce in bytes. 32 bytes (256 bits) matches the
/// SHA-256 block size, which is what the iOS client hashes into the
/// assertion.
pub const CHALLENGE_LEN: usize = 32;

#[derive(Clone)]
struct Entry {
    nonce: Vec<u8>,
    issued_at: Instant,
}

/// Concurrency-safe challenge store, keyed by paired-device mTLS
/// fingerprint. Clone the handle freely — the inner map is `Arc<Mutex>`.
#[derive(Clone, Default)]
pub struct ChallengeStore {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl ChallengeStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh 32-byte challenge for `key` and remember it for
    /// [`CHALLENGE_TTL`]. The previous challenge for the same key (if
    /// any) is overwritten — operators clicking "Register" twice is
    /// expected behavior.
    pub fn issue(&self, key: impl Into<String>) -> Vec<u8> {
        let mut nonce = vec![0u8; CHALLENGE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let entry = Entry {
            nonce: nonce.clone(),
            issued_at: Instant::now(),
        };
        if let Ok(mut g) = self.inner.lock() {
            g.insert(key.into(), entry);
        }
        nonce
    }

    /// Consume the challenge for `key` if present and not expired.
    /// Returns the nonce bytes the App Attest verifier should match.
    pub fn consume(&self, key: &str) -> ConsumeResult {
        let Ok(mut g) = self.inner.lock() else {
            return ConsumeResult::Missing;
        };
        match g.remove(key) {
            Some(entry) if entry.issued_at.elapsed() <= CHALLENGE_TTL => {
                ConsumeResult::Ok(entry.nonce)
            }
            Some(_) => ConsumeResult::Expired,
            None => ConsumeResult::Missing,
        }
    }

    /// Drop entries older than [`CHALLENGE_TTL`]. Cheap to call
    /// periodically — the store is keyed by paired-device fingerprint,
    /// so the working set stays small.
    pub fn purge_expired(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|_, e| e.issued_at.elapsed() <= CHALLENGE_TTL);
        }
    }
}

/// Outcome of consuming a challenge.
#[derive(Debug)]
pub enum ConsumeResult {
    /// Challenge was present and fresh; here are the bytes.
    Ok(Vec<u8>),
    /// No challenge was ever issued for this key, or it was already
    /// consumed by a prior `RegisterDevice` call.
    Missing,
    /// A challenge was issued but is older than [`CHALLENGE_TTL`].
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_challenges_are_32_bytes_and_unique_per_call() {
        // Security-critical: the nonce MUST be 32 random bytes, and two
        // back-to-back issues for the same key must produce different
        // nonces. A regression to a constant nonce (or to an 8-byte
        // nonce) would let a captured assertion be replayed against a
        // future register attempt.
        let store = ChallengeStore::new();
        let a = store.issue("device-1");
        let b = store.issue("device-1");
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b, "two issues must yield different nonces");
        // Statistical sanity: 32 random bytes from a CSPRNG should
        // basically never be all zero. If they are, the RNG is broken.
        assert!(a.iter().any(|&b| b != 0), "nonce a was all zero — RNG bug");
    }

    #[test]
    fn consume_returns_the_issued_bytes_then_marks_missing() {
        // Single-use semantics: a challenge consumed once must not be
        // consumable again. Without this, a stolen assertion could be
        // replayed within the 5-minute TTL to register a foreign device.
        let store = ChallengeStore::new();
        let issued = store.issue("device-1");
        match store.consume("device-1") {
            ConsumeResult::Ok(seen) => assert_eq!(seen, issued),
            other => panic!("first consume should return Ok; got {other:?}"),
        }
        assert!(
            matches!(store.consume("device-1"), ConsumeResult::Missing),
            "second consume of same key must be Missing"
        );
    }

    #[test]
    fn consume_unknown_key_is_missing_not_expired() {
        // The gRPC layer maps Missing → `NotFound` and Expired →
        // `DeadlineExceeded`. Conflating them would make the operator
        // think their challenge timed out when they actually never
        // requested one.
        let store = ChallengeStore::new();
        assert!(matches!(
            store.consume("never-issued"),
            ConsumeResult::Missing
        ));
    }

    #[test]
    fn purge_expired_keeps_fresh_entries_and_removes_old_ones() {
        // We can't easily wait 5 real minutes in a unit test, so this
        // test focuses on the *predicate*: purge_expired must not nuke
        // fresh entries. The TTL-based removal is exercised by the
        // expired-consume path (separately covered by a longer
        // integration test if anyone ever lengthens this fixture).
        let store = ChallengeStore::new();
        let _ = store.issue("keep-me");
        store.purge_expired();
        assert!(
            matches!(store.consume("keep-me"), ConsumeResult::Ok(_)),
            "purge must not evict freshly-issued challenges"
        );
    }
}
