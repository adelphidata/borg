//! Bond system — operational trust tracking between user and agent.
//!
//! Bond score is event-sourced: derived by replaying verified events from a
//! baseline of 40. HMAC chain prevents tampering; rate limiting caps impact
//! per event type per hour to prevent gaming.
//!
//! The HMAC secret is compiled into the binary. This prevents casual SQL
//! tampering but is not cryptographically secure against a determined user
//! who can inspect the binary. The autonomy tier is informational only and
//! does not bypass HITL safety checks.

mod format;
mod heuristics;
mod hook;
mod metrics;
mod replay;
mod types;

pub use format::{format_context, format_history, format_status};
pub use heuristics::{looks_like_acceptance, looks_like_rejection};
pub use hook::BondHook;
pub use metrics::{
    compute_correction_rate, compute_preference_learning_count, compute_routine_success_rate,
};
pub use replay::{replay_events, replay_events_with_key, BOND_HMAC_DOMAIN};

pub(crate) use replay::{compute_event_hmac, rate_limit_for};
pub use types::{
    autonomy_from_level, level_from_score, AutonomyTier, BondEvent, BondLevel, BondState,
};

#[cfg(test)]
mod tests {
    use super::replay::compute_event_hmac;
    use super::types::BASELINE_SCORE;
    use super::*;

    // This module covers only what the dedicated per-file test modules do
    // NOT: the public `types::{level_from_score, autonomy_from_level}`
    // derivations, the hand-written `Display` impls, and the legacy-vs-derived
    // HMAC-key divergence. HMAC chaining, rate limiting, score clamping, tamper
    // detection, heuristics, and formatting are exercised end-to-end in
    // `replay.rs`, `heuristics.rs`, and `format.rs` against the identical code
    // paths (`replay_events`/`replay_events_with_key` with the same key), so
    // duplicating them here would only rubber-stamp.

    // ── Level / Autonomy ──

    #[test]
    fn test_level_from_score_boundaries() {
        assert_eq!(level_from_score(0), BondLevel::Fragile);
        assert_eq!(level_from_score(24), BondLevel::Fragile);
        assert_eq!(level_from_score(25), BondLevel::Emerging);
        assert_eq!(level_from_score(44), BondLevel::Emerging);
        assert_eq!(level_from_score(45), BondLevel::Stable);
        assert_eq!(level_from_score(64), BondLevel::Stable);
        assert_eq!(level_from_score(65), BondLevel::Trusted);
        assert_eq!(level_from_score(84), BondLevel::Trusted);
        assert_eq!(level_from_score(85), BondLevel::Synced);
        assert_eq!(level_from_score(100), BondLevel::Synced);
    }

    #[test]
    fn test_autonomy_from_level() {
        assert_eq!(
            autonomy_from_level(BondLevel::Fragile),
            AutonomyTier::ObserveOnly
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Emerging),
            AutonomyTier::Recommend
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Stable),
            AutonomyTier::DraftAssist
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Trusted),
            AutonomyTier::GuidedAction
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Synced),
            AutonomyTier::HighTrust
        );
    }

    // ── Display ──

    #[test]
    fn test_display_bond_level() {
        assert_eq!(format!("{}", BondLevel::Fragile), "Fragile");
        assert_eq!(format!("{}", BondLevel::Emerging), "Emerging");
        assert_eq!(format!("{}", BondLevel::Stable), "Stable");
        assert_eq!(format!("{}", BondLevel::Trusted), "Trusted");
        assert_eq!(format!("{}", BondLevel::Synced), "Synced");
    }

    #[test]
    fn test_display_autonomy_tier() {
        assert_eq!(format!("{}", AutonomyTier::ObserveOnly), "ObserveOnly");
        assert_eq!(format!("{}", AutonomyTier::Recommend), "Recommend");
        assert_eq!(format!("{}", AutonomyTier::DraftAssist), "DraftAssist");
        assert_eq!(format!("{}", AutonomyTier::GuidedAction), "GuidedAction");
        assert_eq!(format!("{}", AutonomyTier::HighTrust), "HighTrust");
    }

    // ── Anti-tamper hardening: legacy vs derived key divergence ──

    /// The only thing this test covers that `replay.rs` does not: an event
    /// chain signed with a *derived* key must verify under that key but be
    /// REJECTED (chain_valid=false, score unchanged) when replayed under the
    /// legacy compiled-in key via the public `replay_events` entry point.
    /// This pins the key-binding boundary — a regression that ignored the key
    /// would let a derived-key chain pass legacy verification.
    #[test]
    fn test_replay_with_derived_key() {
        let derived_key = b"derived-test-key-for-testing";
        let hmac1 = compute_event_hmac(derived_key, "0", "tool_success", 1, "test", 1000);
        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];
        let state = replay_events_with_key(derived_key, &events);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8 + 1);
        let state_legacy = replay_events(&events);
        assert!(!state_legacy.chain_valid);
        assert_eq!(state_legacy.score, BASELINE_SCORE as u8);
    }
}
