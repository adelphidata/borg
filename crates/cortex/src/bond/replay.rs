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

#[cfg(test)]
mod tests {
    use super::super::types::BASELINE_SCORE;
    use super::*;

    const TEST_KEY: &[u8] = b"bond-replay-test-key";
    /// One hour, used to space events into separate rate-limit buckets.
    const HOUR: i64 = 3_600;

    /// Build a chain of valid events, computing each HMAC against the
    /// running prev_hmac so the whole chain verifies.
    fn build_chain(specs: &[(&str, i32, &str, i64)]) -> Vec<BondEvent> {
        let mut prev = "0".to_string();
        let mut out = Vec::with_capacity(specs.len());
        for (i, (kind, delta, reason, ts)) in specs.iter().enumerate() {
            let h = compute_event_hmac(TEST_KEY, &prev, kind, *delta, reason, *ts);
            out.push(BondEvent {
                id: i as i64 + 1,
                event_type: (*kind).to_string(),
                score_delta: *delta,
                reason: (*reason).to_string(),
                hmac: h.clone(),
                prev_hmac: prev.clone(),
                created_at: *ts,
            });
            prev = h;
        }
        out
    }

    #[test]
    fn empty_events_yields_baseline_with_valid_chain() {
        let s = replay_events_with_key(TEST_KEY, &[]);
        assert_eq!(s.score, BASELINE_SCORE as u8);
        assert!(s.chain_valid);
        assert_eq!(s.total_events, 0);
    }

    #[test]
    fn single_valid_event_applies_delta() {
        let chain = build_chain(&[("tool_success", 1, "x", 1_000)]);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert_eq!(s.score, BASELINE_SCORE as u8 + 1);
        assert!(s.chain_valid);
        assert_eq!(s.total_events, 1);
    }

    #[test]
    fn score_clamps_at_zero_no_underflow() {
        // 50 negative events of -1 — would drive score to -25 if unclamped.
        // Spread across hours to dodge the per-type cap; pick correction (-2).
        let mut specs = Vec::new();
        for h in 0..50 {
            // correction is rate_limit_for=3 per hour, so 1 per hour is safe.
            specs.push(("correction", -2, "x", h * HOUR));
        }
        let chain = build_chain(&specs);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert_eq!(s.score, 0, "score must clamp at 0, got {}", s.score);
        assert!(s.chain_valid);
    }

    #[test]
    fn score_clamps_at_one_hundred() {
        // Need to push past 100. Use tool_success (cap 8/hr, +1/event) and
        // spread across hours; total cap is 15/hr and positive cap is 8/hr.
        let mut specs = Vec::new();
        for h in 0..20 {
            for _ in 0..8 {
                specs.push(("tool_success", 1, "x", h * HOUR));
            }
        }
        let chain = build_chain(&specs);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert_eq!(s.score, 100, "score must clamp at 100, got {}", s.score);
    }

    /// Docstring invariant: a broken HMAC link invalidates the rest of the
    /// chain — all subsequent events are skipped, even if individually valid.
    #[test]
    fn broken_hmac_link_invalidates_tail() {
        let mut chain = build_chain(&[
            ("tool_success", 1, "a", 1_000),
            ("tool_success", 1, "b", 2_000),
            ("tool_success", 1, "c", 3_000),
        ]);
        // Tamper the middle event's stored hmac. The tail's prev_hmac was
        // computed from the *original* hmac, so the link still chains
        // structurally — but verification recomputes and rejects it.
        chain[1].hmac = "deadbeef".to_string();
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert!(!s.chain_valid, "tampered hmac must mark chain invalid");
        // Only the first event scored; the tampered one and everything after
        // it were skipped. Baseline + 1.
        assert_eq!(s.score, BASELINE_SCORE as u8 + 1);
        // total_events still reports the raw count (display purposes).
        assert_eq!(s.total_events, 3);
    }

    #[test]
    fn tampered_prev_hmac_invalidates_event() {
        let mut chain = build_chain(&[
            ("tool_success", 1, "a", 1_000),
            ("tool_success", 1, "b", 2_000),
        ]);
        chain[1].prev_hmac = "0".to_string(); // wrong — should be event[0].hmac
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert!(!s.chain_valid);
        assert_eq!(s.score, BASELINE_SCORE as u8 + 1);
    }

    /// Defense-in-depth: an event with a verified HMAC but an event_type not
    /// in the allowlist must be skipped silently — chain stays valid.
    #[test]
    fn unknown_event_type_is_skipped_chain_stays_valid() {
        let chain = build_chain(&[
            ("tool_success", 1, "a", 1_000),
            ("ghost_event", 99, "b", 2_000), // verified but unknown type
            ("tool_success", 1, "c", 3_000),
        ]);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert!(
            s.chain_valid,
            "verified-but-unknown event should not break chain"
        );
        // ghost_event's huge +99 delta must NOT be applied. Only the two
        // valid tool_success events score: baseline + 2.
        assert_eq!(s.score, BASELINE_SCORE as u8 + 2);
    }

    /// `tool_success` cap is 8/hour. The 9th event in the same hour must not
    /// score, but its chain link still advances so the *next hour* event
    /// scores normally.
    #[test]
    fn per_type_rate_limit_caps_within_hour_resets_next_hour() {
        let mut specs = Vec::new();
        // 9 successes in hour 0
        for _ in 0..9 {
            specs.push(("tool_success", 1, "x", HOUR));
        }
        // 1 success in hour 1
        specs.push(("tool_success", 1, "x", 2 * HOUR));
        let chain = build_chain(&specs);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert!(s.chain_valid);
        // 8 (cap) + 1 (next hour) = 9 successes scored.
        assert_eq!(s.score, BASELINE_SCORE as u8 + 9);
    }

    /// Hourly positive cap (8): 9 positive events in one hour → 8 score.
    /// Negative events in the same hour bypass the positive cap.
    #[test]
    fn hourly_positive_cap_applies_to_positives_only() {
        let mut specs = Vec::new();
        // 9 positives — mix types so per-type cap (3 for creation, 8 for
        // tool_success) doesn't bite first.
        for _ in 0..8 {
            specs.push(("tool_success", 1, "x", HOUR));
        }
        specs.push(("creation", 1, "x", HOUR)); // 9th positive — capped
                                                // A negative in same hour — must still apply.
        specs.push(("correction", -2, "x", HOUR));
        let chain = build_chain(&specs);
        let s = replay_events_with_key(TEST_KEY, &chain);
        // 8 successes + 1 capped + (-2 correction) = baseline + 6
        assert_eq!(s.score, BASELINE_SCORE as u8 + 8 - 2);
    }

    /// Hourly total cap (15): 16 events in one hour → 16th rejected.
    /// We use only correction (-2) events spread to dodge the per-type cap of
    /// 3 by mixing types — but to drive past total cap we just need 16 in one
    /// hour with score impact small enough not to confuse the test.
    #[test]
    fn hourly_total_cap_blocks_excess_events() {
        // Build 15 valid scoring events (mix to dodge per-type caps) + 1 more.
        let mut specs = Vec::new();
        // 8 tool_success (cap 8 — fills positive + per-type)
        for _ in 0..8 {
            specs.push(("tool_success", 1, "ok", HOUR));
        }
        // 5 tool_failure (cap 5, negative — bypasses positive cap)
        for _ in 0..5 {
            specs.push(("tool_failure", -1, "f", HOUR));
        }
        // 2 corrections (cap 3, negative)
        for _ in 0..2 {
            specs.push(("correction", -2, "c", HOUR));
        }
        // That's 15 events scoring. Add 1 more correction in the same hour
        // (still within the per-type cap of 3) — it must be rejected by the
        // total cap, NOT by the per-type cap.
        specs.push(("correction", -2, "c", HOUR));
        let chain = build_chain(&specs);
        let s = replay_events_with_key(TEST_KEY, &chain);
        assert!(s.chain_valid);
        // Score: +8 -5 -4 = -1 → baseline 25 - 1 = 24. The 16th correction
        // (-2) was rejected by the total cap, otherwise score would be 22.
        assert_eq!(
            s.score,
            (BASELINE_SCORE + 8 - 5 - 4) as u8,
            "16th event should be rejected by total cap"
        );
    }

    #[test]
    fn rate_limit_for_pins_documented_caps() {
        // Catches accidental table edits.
        assert_eq!(rate_limit_for("tool_success"), 8);
        assert_eq!(rate_limit_for("tool_failure"), 5);
        assert_eq!(rate_limit_for("creation"), 3);
        assert_eq!(rate_limit_for("correction"), 3);
        assert_eq!(rate_limit_for("suggestion_accepted"), 3);
        assert_eq!(rate_limit_for("suggestion_rejected"), 3);
        // Unknown defaults to 5.
        assert_eq!(rate_limit_for("not_a_real_type"), 5);
        assert_eq!(rate_limit_for(""), 5);
    }

    #[test]
    fn compute_event_hmac_is_deterministic() {
        let a = compute_event_hmac(TEST_KEY, "p", "tool_success", 1, "r", 1_000);
        let b = compute_event_hmac(TEST_KEY, "p", "tool_success", 1, "r", 1_000);
        assert_eq!(a, b);
    }

    /// Flipping any single field must change the HMAC. Catches future edits
    /// to `compute_event_hmac` that accidentally drop a field from the input.
    #[test]
    fn compute_event_hmac_diffuses_each_field() {
        let base = compute_event_hmac(TEST_KEY, "p", "tool_success", 1, "r", 1_000);
        assert_ne!(
            base,
            compute_event_hmac(TEST_KEY, "p2", "tool_success", 1, "r", 1_000),
            "prev_hmac must affect output"
        );
        assert_ne!(
            base,
            compute_event_hmac(TEST_KEY, "p", "tool_failure", 1, "r", 1_000),
            "event_type must affect output"
        );
        assert_ne!(
            base,
            compute_event_hmac(TEST_KEY, "p", "tool_success", 2, "r", 1_000),
            "delta must affect output"
        );
        assert_ne!(
            base,
            compute_event_hmac(TEST_KEY, "p", "tool_success", 1, "r2", 1_000),
            "reason must affect output"
        );
        assert_ne!(
            base,
            compute_event_hmac(TEST_KEY, "p", "tool_success", 1, "r", 1_001),
            "created_at must affect output"
        );
        assert_ne!(
            base,
            compute_event_hmac(b"different-key", "p", "tool_success", 1, "r", 1_000),
            "key must affect output"
        );
    }
}
