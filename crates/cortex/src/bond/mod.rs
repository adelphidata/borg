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
const BOND_HMAC_LEGACY: &[u8] = b"borg-bond-chain-v1";

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::format::{bar, format_ago};
    use super::replay::compute_event_hmac;
    use super::types::BASELINE_SCORE;
    use super::*;

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

    // ── HMAC ──

    #[test]
    fn test_compute_event_hmac_deterministic() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn test_compute_event_hmac_different_inputs() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_failure", -1, "run_shell", 1000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_compute_event_hmac_different_prev() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "abc", "tool_success", 1, "test", 1000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_verify_chain_valid() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let hmac2 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac1,
            "creation",
            1,
            "write_memory",
            2000,
        );
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac2,
            "tool_failure",
            -1,
            "run_shell",
            3000,
        );

        let events = vec![
            BondEvent {
                id: 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac1.clone(),
                prev_hmac: "0".to_string(),
                created_at: 1000,
            },
            BondEvent {
                id: 2,
                event_type: "creation".to_string(),
                score_delta: 1,
                reason: "write_memory".to_string(),
                hmac: hmac2.clone(),
                prev_hmac: hmac1,
                created_at: 2000,
            },
            BondEvent {
                id: 3,
                event_type: "tool_failure".to_string(),
                score_delta: -1,
                reason: "run_shell".to_string(),
                hmac: hmac3,
                prev_hmac: hmac2,
                created_at: 3000,
            },
        ];

        let state = replay_events(&events);
        assert!(state.chain_valid);
        // 25 + 1 + 1 - 1 = 26
        assert_eq!(state.score, 26);
        assert_eq!(state.level, BondLevel::Emerging);
    }

    #[test]
    fn test_verify_chain_tampered_delta() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);

        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 99, // Tampered delta (was 1)
            reason: "test".to_string(),
            hmac: hmac1,
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];

        let state = replay_events(&events);
        assert!(!state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8);
    }

    #[test]
    fn test_verify_chain_mid_corruption_invalidates_tail() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let hmac2_real = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac1,
            "creation",
            1,
            "write_memory",
            2000,
        );
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac2_real,
            "tool_success",
            1,
            "read_file",
            3000,
        );

        let events = vec![
            BondEvent {
                id: 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac1.clone(),
                prev_hmac: "0".to_string(),
                created_at: 1000,
            },
            BondEvent {
                id: 2,
                event_type: "creation".to_string(),
                score_delta: 99, // Tampered (was 1)
                reason: "write_memory".to_string(),
                hmac: hmac2_real.clone(),
                prev_hmac: hmac1,
                created_at: 2000,
            },
            BondEvent {
                id: 3,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "read_file".to_string(),
                hmac: hmac3,
                prev_hmac: hmac2_real,
                created_at: 3000,
            },
        ];

        let state = replay_events(&events);
        assert!(!state.chain_valid);
        assert_eq!(state.score, 26);
    }

    #[test]
    fn test_verify_chain_empty() {
        let state = replay_events(&[]);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8);
        assert_eq!(state.level, BondLevel::Emerging);
    }

    // ── Rate Limiting ──

    #[test]
    fn test_rate_limiting_caps_events() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let base_time = 3600 * 1000;

        for i in 0..20 {
            let created_at = base_time + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        // Rate limit for tool_success is 8 per hour; 25 + 8 = 33
        assert_eq!(state.score, 33);
        assert!(state.chain_valid);
    }

    #[test]
    fn test_rate_limiting_different_hours_not_capped() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..20 {
            let created_at = 3600 * (i as i64 + 1);
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        // 25 + 20 = 45
        assert_eq!(state.score, 45);
    }

    // ── Replay Clamping ──

    #[test]
    fn test_replay_clamp_upper() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..80 {
            let created_at = 3600 * (i as i64 + 1);
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "creation",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "creation".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        assert_eq!(state.score, 100);
    }

    #[test]
    fn test_replay_clamp_lower() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..50 {
            let created_at = 3600 * (i as i64 + 1);
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "correction",
                -2,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "correction".to_string(),
                score_delta: -2,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        assert_eq!(state.score, 0);
    }

    // ── Heuristics ──

    #[test]
    fn test_acceptance_positive() {
        assert!(looks_like_acceptance("yes, do it"));
        assert!(looks_like_acceptance("Go ahead"));
        assert!(looks_like_acceptance("sounds good to me"));
        assert!(looks_like_acceptance("approved"));
        assert!(looks_like_acceptance("let's do it"));
        assert!(looks_like_acceptance("yes please"));
        assert!(looks_like_acceptance("absolutely"));
    }

    #[test]
    fn test_acceptance_negative() {
        assert!(!looks_like_acceptance("can you help me?"));
        assert!(!looks_like_acceptance("what does this do?"));
        assert!(!looks_like_acceptance("show me the code"));
        assert!(!looks_like_acceptance("that's wrong"));
        assert!(!looks_like_acceptance("yes, what is the status?"));
        assert!(!looks_like_acceptance("great work on that"));
        assert!(!looks_like_acceptance("ok so what happened?"));
        assert!(!looks_like_acceptance("sure, can you explain?"));
    }

    #[test]
    fn test_rejection_positive() {
        assert!(looks_like_rejection("no don't do that"));
        assert!(looks_like_rejection("don't do it"));
        assert!(looks_like_rejection("cancel that"));
        assert!(looks_like_rejection("never mind"));
        assert!(looks_like_rejection("skip that"));
        assert!(looks_like_rejection("stop suggesting things"));
    }

    #[test]
    fn test_rejection_negative() {
        assert!(!looks_like_rejection("yes please"));
        assert!(!looks_like_rejection("sounds good"));
        assert!(!looks_like_rejection("help me with this"));
        assert!(!looks_like_rejection("I don't understand"));
    }

    // ── Formatting ──

    #[test]
    fn test_format_context_compact() {
        let state = BondState {
            score: 68,
            level: BondLevel::Trusted,
            autonomy_tier: AutonomyTier::GuidedAction,
            total_events: 100,
            chain_valid: true,
        };
        let ctx = format_context(&state, 0.09, 0.83);
        assert!(ctx.contains("Trusted"));
        assert!(ctx.contains("68"));
        assert!(ctx.contains("GuidedAction"));
        assert!(ctx.contains("bond_context"));
        assert!(ctx.len() < 200);
    }

    #[test]
    fn test_format_status_structure() {
        let state = BondState {
            score: 40,
            level: BondLevel::Emerging,
            autonomy_tier: AutonomyTier::Recommend,
            total_events: 0,
            chain_valid: true,
        };
        let output = format_status(&state, 0.05, 0.80, 3, &[]);
        assert!(output.contains("Bond Status"));
        assert!(output.contains("Emerging"));
        assert!(output.contains("Recommend"));
        assert!(output.contains("Chain valid"));
        assert!(output.contains("80%"));
        assert!(output.contains("5%"));
        assert!(output.contains("Tip:"));
    }

    #[test]
    fn test_format_status_broken_chain() {
        let state = BondState {
            score: 40,
            level: BondLevel::Emerging,
            autonomy_tier: AutonomyTier::Recommend,
            total_events: 5,
            chain_valid: false,
        };
        let output = format_status(&state, 0.0, 0.0, 0, &[]);
        assert!(output.contains("Chain broken"));
    }

    #[test]
    fn test_format_history_empty() {
        let output = format_history(&[]);
        assert!(output.contains("No bond events"));
    }

    #[test]
    fn test_format_history_with_events() {
        let now = Utc::now().timestamp();
        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "run_shell".to_string(),
            hmac: "abc".to_string(),
            prev_hmac: "0".to_string(),
            created_at: now - 120,
        }];
        let output = format_history(&events);
        assert!(output.contains("tool_success"));
        assert!(output.contains("+1"));
        assert!(output.contains("run_shell"));
        assert!(output.contains("2m ago"));
    }

    #[test]
    fn test_format_ago_negative() {
        assert_eq!(format_ago(-100), "in the future");
    }

    #[test]
    fn test_format_ago_ranges() {
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(30), "just now");
        assert_eq!(format_ago(120), "2m ago");
        assert_eq!(format_ago(7200), "2h ago");
        assert_eq!(format_ago(172800), "2d ago");
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

    // ── Bar Rendering ──

    #[test]
    fn test_bar_full() {
        assert_eq!(
            bar(100, 10),
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}"
        );
    }

    #[test]
    fn test_bar_empty() {
        assert_eq!(
            bar(0, 10),
            "\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}"
        );
    }

    #[test]
    fn test_bar_half() {
        let b = bar(50, 10);
        assert_eq!(b.chars().count(), 10);
    }

    // ── Anti-tamper hardening tests ──

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

    #[test]
    fn test_total_hourly_rate_limit() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 100;
        for i in 0..35i64 {
            let (event_type, delta) = match i % 3 {
                0 => ("tool_success", 1),
                1 => ("creation", 2),
                _ => ("suggestion_accepted", 1),
            };
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                event_type,
                delta,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: event_type.to_string(),
                score_delta: delta,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        let all_sum: i32 = events.iter().map(|e| e.score_delta).sum();
        assert!((state.score as i32) < BASELINE_SCORE + all_sum);
    }

    #[test]
    fn test_positive_delta_hourly_rate_limit() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 200;
        for i in 0..20i64 {
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8 + 8);
    }

    #[test]
    fn test_invalid_event_type_skipped_in_replay() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "custom_exploit", 99, "test", 1000);
        let events = vec![BondEvent {
            id: 1,
            event_type: "custom_exploit".to_string(),
            score_delta: 99,
            reason: "test".to_string(),
            hmac: hmac1,
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];
        let state = replay_events(&events);
        assert_eq!(state.score, BASELINE_SCORE as u8);
    }

    #[test]
    fn test_negative_events_bypass_positive_cap() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 300;
        for i in 0..15i64 {
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        for i in 0..10i64 {
            let created_at = hour_base + 15 + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_failure",
                -1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: 15 + i + 1,
                event_type: "tool_failure".to_string(),
                score_delta: -1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(state.score, 28); // 25 + 8 - 5
    }

    #[test]
    fn test_inject_context_format() {
        let state = BondState {
            score: 68,
            level: BondLevel::Trusted,
            autonomy_tier: AutonomyTier::GuidedAction,
            total_events: 100,
            chain_valid: true,
        };
        let ctx = format_context(&state, 0.09, 0.83);
        assert!(ctx.contains("<bond_context>"));
        assert!(ctx.contains("</bond_context>"));
        assert!(ctx.contains("Trusted"));
        assert!(ctx.contains("68/100"));
        assert!(ctx.contains("GuidedAction"));
        assert!(ctx.contains("9%"));
        assert!(ctx.contains("83%"));
    }

    #[test]
    fn test_mid_chain_corruption_invalidates_tail() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let event1 = BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
            created_at: 1000,
        };
        let event2 = BondEvent {
            id: 2,
            event_type: "creation".to_string(),
            score_delta: 2,
            reason: "test".to_string(),
            hmac: "corrupted".to_string(),
            prev_hmac: hmac1,
            created_at: 2000,
        };
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            "corrupted",
            "tool_success",
            1,
            "test",
            3000,
        );
        let event3 = BondEvent {
            id: 3,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac3,
            prev_hmac: "corrupted".to_string(),
            created_at: 3000,
        };

        let state = replay_events(&[event1, event2, event3]);
        assert_eq!(state.score, 26);
        assert!(!state.chain_valid);
    }

    #[test]
    fn empty_events_gives_baseline_score() {
        let state = replay_events(&[]);
        assert_eq!(state.score, BASELINE_SCORE as u8);
        assert!(state.chain_valid, "empty chain should be considered valid");
        assert_eq!(state.level, level_from_score(BASELINE_SCORE as u8));
    }
}
