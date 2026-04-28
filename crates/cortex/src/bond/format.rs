//! Presentation layer for bond status output. Produces the compact context
//! string injected into the system prompt and the larger reports rendered by
//! `borg bond` / `borg bond history`.

use chrono::Utc;

use super::types::{BondEvent, BondLevel, BondState};

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
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
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
