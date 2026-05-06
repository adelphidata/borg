//! Presentation layer for bond status output. Produces the compact context
//! string injected into the system prompt and the larger reports rendered by
//! `borg bond` / `borg bond history`.

use chrono::Utc;

use super::types::{BondEvent, BondLevel, BondState};
use crate::constants::SECS_PER_DAY;

pub(super) fn bar(val: u8, width: usize) -> String {
    let filled = (val as usize * width + 50) / 100;
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Compact XML context for system prompt injection (~60 tokens).
pub fn format_context(state: &BondState, correction_rate: f32, routine_rate: f32) -> String {
    format!(
        "<bond_context>\nBond: {} ({}/100) | Autonomy: {}\nCorrection rate: {:.0}% | Routine success: {:.0}%\n</bond_context>",
        state.level,
        state.score,
        state.autonomy_tier,
        correction_rate * 100.0,
        routine_rate * 100.0,
    )
}

/// Full status output for `borg bond`.
pub fn format_status(
    state: &BondState,
    correction_rate: f32,
    routine_rate: f32,
    pref_count: u32,
    recent_events: &[BondEvent],
) -> String {
    let mut out = String::from(
        "Bond Status\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  (trust your borg has earned; higher score unlocks more autonomy)\n",
    );

    out.push_str(&format!(
        "  score        {}  {}\n",
        bar(state.score, 10),
        state.score
    ));
    out.push_str(&format!("  level        {}\n", state.level));
    out.push_str(&format!("  autonomy     {}\n", state.autonomy_tier));

    let integrity = if state.chain_valid {
        format!("Chain valid ({} events)", state.total_events)
    } else {
        format!("Chain broken ({} events)", state.total_events)
    };
    out.push_str(&format!("  integrity    {integrity}\n"));

    out.push_str("\n30d Signals\n");
    out.push_str(&format!(
        "  Routine Success Rate   {:.0}%\n",
        routine_rate * 100.0
    ));
    out.push_str(&format!(
        "  Correction Rate        {:.0}%\n",
        correction_rate * 100.0
    ));
    out.push_str(&format!("  Preferences Learned    {pref_count}\n"));

    if !recent_events.is_empty() {
        out.push_str("\nRecent Events\n");
        let now = Utc::now().timestamp();
        for event in recent_events.iter().take(5) {
            let ago = format_ago(now - event.created_at);
            let delta_str = if event.score_delta >= 0 {
                format!("+{}", event.score_delta)
            } else {
                format!("{}", event.score_delta)
            };
            out.push_str(&format!(
                "  {:<22} {:>3}  {:<20} {}\n",
                event.event_type, delta_str, event.reason, ago
            ));
        }
    }

    let tip = recommend(state);
    out.push_str(&format!("\nTip: {tip}\n"));

    out
}

/// Tabular event history for `borg bond history`.
pub fn format_history(events: &[BondEvent]) -> String {
    if events.is_empty() {
        return "No bond events recorded yet.\n".to_string();
    }

    let mut out = format!(
        "{:<6} {:<22} {:>6} {:<20} {}\n",
        "ID", "Type", "Delta", "Reason", "Time"
    );
    out.push_str(&"\u{2500}".repeat(70));
    out.push('\n');

    let now = Utc::now().timestamp();
    for event in events {
        let ago = format_ago(now - event.created_at);
        let delta_str = if event.score_delta >= 0 {
            format!("+{}", event.score_delta)
        } else {
            format!("{}", event.score_delta)
        };
        out.push_str(&format!(
            "{:<6} {:<22} {:>6} {:<20} {}\n",
            event.id, event.event_type, delta_str, event.reason, ago
        ));
    }
    out
}

pub(super) fn format_ago(seconds: i64) -> String {
    if seconds < 0 {
        "in the future".to_string()
    } else if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < SECS_PER_DAY {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / SECS_PER_DAY)
    }
}

pub(super) fn recommend(state: &BondState) -> &'static str {
    match state.level {
        BondLevel::Fragile => "Complete successful tasks to rebuild trust",
        BondLevel::Emerging => "Keep using borg consistently to strengthen trust",
        BondLevel::Stable => "Trust is building — try creating tools or skills to grow",
        BondLevel::Trusted => "Strong trust established — keep completing tasks successfully",
        BondLevel::Synced => "Maximum trust achieved — maintain regular usage",
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{autonomy_from_level, level_from_score, BondEvent, BondState};
    use super::*;

    fn state_at(score: u8, total_events: u32, chain_valid: bool) -> BondState {
        let level = level_from_score(score);
        BondState {
            score,
            level,
            autonomy_tier: autonomy_from_level(level),
            total_events,
            chain_valid,
        }
    }

    fn event(id: i64, kind: &str, delta: i32, reason: &str, age_secs: i64) -> BondEvent {
        BondEvent {
            id,
            event_type: kind.to_string(),
            score_delta: delta,
            reason: reason.to_string(),
            hmac: String::new(),
            prev_hmac: String::new(),
            created_at: chrono::Utc::now().timestamp() - age_secs,
        }
    }

    #[test]
    fn bar_endpoints_and_rounding() {
        // ASCII byte counting is wrong for these multi-byte glyphs; count chars.
        let count_chars = |s: &str| s.chars().count();

        let zero = bar(0, 10);
        assert_eq!(count_chars(&zero), 10);
        // All blocks at val=0 must be the empty glyph.
        assert!(zero.chars().all(|c| c == '\u{2591}'));

        let full = bar(100, 10);
        assert_eq!(count_chars(&full), 10);
        assert!(full.chars().all(|c| c == '\u{2588}'));

        // Half-up rounding: (50*10+50)/100 = 5
        let half = bar(50, 10);
        assert_eq!(half.chars().filter(|c| *c == '\u{2588}').count(), 5);
        assert_eq!(half.chars().filter(|c| *c == '\u{2591}').count(), 5);

        // (45*10+50)/100 = 5 — proves the +50 half-up bias
        let bias = bar(45, 10);
        assert_eq!(bias.chars().filter(|c| *c == '\u{2588}').count(), 5);

        // (44*10+50)/100 = 4 — just below the bias threshold
        let below = bar(44, 10);
        assert_eq!(below.chars().filter(|c| *c == '\u{2588}').count(), 4);
        assert_eq!(below.chars().filter(|c| *c == '\u{2591}').count(), 6);
    }

    #[test]
    fn format_ago_boundaries() {
        assert_eq!(format_ago(-1), "in the future");
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(59), "just now");
        assert_eq!(format_ago(60), "1m ago");
        assert_eq!(format_ago(3599), "59m ago");
        assert_eq!(format_ago(3600), "1h ago");
        assert_eq!(format_ago(86399), "23h ago");
        assert_eq!(format_ago(86400), "1d ago");
        assert_eq!(format_ago(100 * 86400), "100d ago");
    }

    #[test]
    fn recommend_per_level() {
        // Pin each user-facing string by level. Catches silent copy edits.
        let cases: &[(u8, &str)] = &[
            (0, "Complete successful tasks to rebuild trust"),
            (24, "Complete successful tasks to rebuild trust"),
            (25, "Keep using borg consistently to strengthen trust"),
            (44, "Keep using borg consistently to strengthen trust"),
            (
                45,
                "Trust is building — try creating tools or skills to grow",
            ),
            (
                64,
                "Trust is building — try creating tools or skills to grow",
            ),
            (
                65,
                "Strong trust established — keep completing tasks successfully",
            ),
            (
                84,
                "Strong trust established — keep completing tasks successfully",
            ),
            (85, "Maximum trust achieved — maintain regular usage"),
            (100, "Maximum trust achieved — maintain regular usage"),
        ];
        for (score, expected) in cases {
            assert_eq!(
                recommend(&state_at(*score, 0, true)),
                *expected,
                "score={score}"
            );
        }
    }

    #[test]
    fn format_history_empty() {
        assert_eq!(format_history(&[]), "No bond events recorded yet.\n");
    }

    #[test]
    fn format_history_renders_positive_and_negative_deltas() {
        let events = vec![
            event(7, "tool_success", 1, "apply_patch", 30),
            event(8, "correction", -2, "user_message", 120),
        ];
        let out = format_history(&events);
        assert!(out.contains("ID"), "missing header: {out}");
        assert!(out.contains("Type"), "missing header column: {out}");
        // Separator line — 70 box-draw chars.
        assert!(out.contains(&"\u{2500}".repeat(70)));
        // Both rows present with correctly-signed delta strings.
        assert!(out.contains("tool_success"), "missing positive row: {out}");
        assert!(out.contains("    +1"), "positive delta not signed: {out}");
        assert!(out.contains("correction"), "missing negative row: {out}");
        assert!(
            out.contains("    -2"),
            "negative delta not formatted: {out}"
        );
        // IDs are rendered.
        assert!(out.contains('7') && out.contains('8'));
    }

    #[test]
    fn format_status_chain_broken_signal() {
        let s = state_at(40, 12, false);
        let out = format_status(&s, 0.1, 0.5, 0, &[]);
        assert!(
            out.contains("Chain broken (12 events)"),
            "broken-chain signal missing: {out}"
        );
        assert!(!out.contains("Chain valid"));
    }

    #[test]
    fn format_status_omits_recent_events_when_empty() {
        let s = state_at(50, 0, true);
        let out = format_status(&s, 0.0, 0.0, 0, &[]);
        assert!(
            !out.contains("Recent Events"),
            "should omit empty section: {out}"
        );
        assert!(out.contains("Chain valid (0 events)"));
        assert!(out.contains("Tip:"), "tip line missing: {out}");
    }

    #[test]
    fn format_status_includes_recent_event_with_negative_delta() {
        let s = state_at(50, 1, true);
        let evs = vec![event(3, "correction", -2, "user_message", 60)];
        let out = format_status(&s, 0.0, 0.0, 0, &evs);
        assert!(out.contains("Recent Events"));
        assert!(out.contains("correction"));
        assert!(out.contains(" -2 "), "negative delta missing in row: {out}");
        assert!(out.contains("1m ago"), "elapsed missing: {out}");
    }

    #[test]
    fn format_context_pins_all_fields() {
        let s = state_at(72, 5, true);
        let out = format_context(&s, 0.123, 0.876);
        // Score, level, autonomy, and rates rounded to integer percent.
        assert!(
            out.contains("Bond: Trusted (72/100)"),
            "score/level missing: {out}"
        );
        assert!(
            out.contains("Autonomy: GuidedAction"),
            "autonomy missing: {out}"
        );
        assert!(
            out.contains("Correction rate: 12%"),
            "correction rate missing: {out}"
        );
        assert!(
            out.contains("Routine success: 88%"),
            "routine rate missing: {out}"
        );
        // XML tag boundaries — agent system prompt parses these.
        assert!(out.starts_with("<bond_context>"));
        assert!(out.ends_with("</bond_context>"));
    }
}
