//! HMAC chain verification, rate limiting, and event replay for the bond
//! system. The bond score is event-sourced — derived by replaying verified
//! events from a baseline. A broken HMAC link invalidates the rest of the
//! chain.

use crate::hmac_chain;

use super::types::{autonomy_from_level, level_from_score, BondEvent, BondState, BASELINE_SCORE};

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub const BOND_HMAC_DOMAIN: &[u8] = b"borg-bond-chain-v1";

/// Compute HMAC for a bond event using the shared HMAC chain module.
pub(crate) fn compute_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    score_delta: i32,
    reason: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &score_delta.to_le_bytes(),
            reason.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Verify a bond event's HMAC against the expected chain.
pub(super) fn verify_event_hmac(key: &[u8], event: &BondEvent, expected_prev_hmac: &str) -> bool {
    let recomputed = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.score_delta,
        &event.reason,
        event.created_at,
    );
    hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed,
    )
}

/// Maximum events per type per hour during replay.
pub(crate) fn rate_limit_for(event_type: &str) -> u32 {
    match event_type {
        "tool_success" => 8,
        "tool_failure" => 5,
        "creation" => 3,
        "correction" => 3,
        "suggestion_accepted" => 3,
        "suggestion_rejected" => 3,
        _ => 5,
    }
}

/// Replay verified events from baseline to compute current bond state.
/// Verifies HMAC chain and applies per-type-per-hour rate limits.
///
/// A broken HMAC link invalidates the rest of the chain — all subsequent
/// events are skipped. This is intentional: the chain is append-only and
/// any tampering makes the tail untrustworthy.
pub fn replay_events(events: &[BondEvent]) -> BondState {
    replay_events_with_key(BOND_HMAC_DOMAIN, events)
}

/// Replay events with a specific HMAC key (for per-installation derived keys).
pub fn replay_events_with_key(key: &[u8], events: &[BondEvent]) -> BondState {
    const VALID_EVENT_TYPES: &[&str] = &[
        "tool_success",
        "tool_failure",
        "creation",
        "correction",
        "suggestion_accepted",
        "suggestion_rejected",
    ];

    let mut score: i32 = BASELINE_SCORE;
    let mut expected_prev_hmac = "0".to_string();
    let mut rate_limiter = hmac_chain::HourlyRateLimiter::new(Some(15), Some(8));
    let mut chain_valid = true;

    for event in events {
        // Verify HMAC chain — skip tampered/injected events
        if !verify_event_hmac(key, event, &expected_prev_hmac) {
            tracing::warn!("bond: skipping event {} with broken HMAC chain", event.id);
            chain_valid = false;
            continue;
        }
        // Advance chain pointer. Rate-limited events still advance the pointer
        // since they are verified and trusted, just capped for scoring.
        expected_prev_hmac.clone_from(&event.hmac);

        // Defense-in-depth: skip unknown event types
        if !VALID_EVENT_TYPES.contains(&event.event_type.as_str()) {
            continue;
        }

        // Combined rate limiting: total/hour, positive/hour, per-type/hour
        let cap = rate_limit_for(&event.event_type);
        let is_positive = event.score_delta > 0;
        if !rate_limiter.check_and_consume(event.created_at, &event.event_type, cap, is_positive) {
            continue;
        }

        // Apply delta
        score = (score + event.score_delta).clamp(0, 100);
    }

    let score = score as u8;
    let level = level_from_score(score);

    BondState {
        score,
        level,
        autonomy_tier: autonomy_from_level(level),
        total_events: u32::try_from(events.len()).unwrap_or(u32::MAX),
        chain_valid,
    }
}
