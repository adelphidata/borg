//! Derived helpers layered on top of the event-replay `EvolutionState`.
//!
//! These functions take combinations of evolution, vitals, and bond state
//! and return UX-facing signals (momentum, mood, readiness, hints) used by
//! the V2 surfaces (`/evolution`, `/xp`, ambient header). They are pure —
//! they never hit the database or record new events.

use std::collections::HashMap;

use super::{
    check_stage1_gates, Archetype, BlockingGate, EvolutionEvent, EvolutionState, Mood,
    ReadinessReport, Stage, Trend,
};
use crate::bond::BondState;
use crate::vitals::VitalsState;

/// 7-day window used for momentum computation.
const WINDOW_7D_SECS: i64 = 7 * 86_400;

/// Minimum XP delta (last window − prior window) for an archetype to be
/// marked Rising or Falling. Below this, the archetype is considered Stable.
const MOMENTUM_TOLERANCE: i64 = 2;

/// Compute per-archetype trend by comparing the last 7 days of aligned XP
/// to the prior 7 days.
///
/// Only `xp_gain` events contribute. Archetypes never seen in either window
/// are omitted from the result (rather than reported Stable, which would be
/// misleading for a never-used archetype).
pub fn compute_momentum(events: &[EvolutionEvent], now: i64) -> HashMap<Archetype, Trend> {
    let recent_cutoff = now - WINDOW_7D_SECS;
    let prior_cutoff = now - 2 * WINDOW_7D_SECS;

    let mut recent: HashMap<Archetype, i64> = HashMap::new();
    let mut prior: HashMap<Archetype, i64> = HashMap::new();

    for e in events {
        if e.event_type != "xp_gain" {
            continue;
        }
        let Some(arch_str) = e.archetype.as_deref() else {
            continue;
        };
        let Some(arch) = Archetype::parse(arch_str) else {
            continue;
        };
        let xp = e.xp_delta.max(0) as i64;
        if e.created_at >= recent_cutoff {
            *recent.entry(arch).or_insert(0) += xp;
        } else if e.created_at >= prior_cutoff {
            *prior.entry(arch).or_insert(0) += xp;
        }
    }

    let mut out = HashMap::new();
    for arch in Archetype::ALL {
        let r = recent.get(&arch).copied().unwrap_or(0);
        let p = prior.get(&arch).copied().unwrap_or(0);
        if r == 0 && p == 0 {
            continue;
        }
        let delta = r - p;
        let trend = if delta > MOMENTUM_TOLERANCE {
            Trend::Rising
        } else if delta < -MOMENTUM_TOLERANCE {
            Trend::Falling
        } else {
            Trend::Stable
        };
        out.insert(arch, trend);
    }
    out
}

/// Derive the companion mood from vitals, bond, and current evolution state.
///
/// The derivation is deliberately simple and stable — a lossy UX signal,
/// not authoritative. Callers should not branch on mood for safety-critical
/// decisions.
pub fn compute_mood(evo: &EvolutionState, vitals: &VitalsState, bond: &BondState) -> Mood {
    let min_vital = vitals
        .stability
        .min(vitals.focus)
        .min(vitals.sync)
        .min(vitals.growth)
        .min(vitals.happiness);

    // Near-evolution dominates: Lvl.99, non-final, bond meeting the floor.
    if evo.level >= 99 && evo.stage != Stage::Final && bond.score >= 30 {
        return Mood::Ascending;
    }

    if min_vital < 30 {
        return Mood::Strained;
    }

    // No dominant archetype yet: drifting.
    if evo.dominant_archetype.is_none() {
        return Mood::Drifting;
    }

    // Growth is the strongest signal of active learning; treat it as
    // Learning only while still in Base (post-Base we've settled enough that
    // "stable" / "focused" is the more honest read).
    if evo.stage == Stage::Base && vitals.growth >= 60 && evo.total_xp > 0 {
        return Mood::Learning;
    }

    if vitals.focus >= 70 && vitals.stability >= 60 {
        return Mood::Focused;
    }

    Mood::Stable
}

/// Render a filled/empty bar: `val` out of `max`, spanning `width` chars.
///
/// Extracted from the duplicated copies in `vitals.rs` and this module's
/// `format_*` functions so they share a single implementation. Clamps `val`
/// to `max` and returns a string whose `chars().count()` is exactly `width`.
pub fn render_bar(val: u32, max: u32, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let max = max.max(1);
    let clamped = val.min(max);
    // +max/2 rounds to nearest filled cell.
    let filled = ((clamped as u64 * width as u64 + (max as u64) / 2) / max as u64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Compute a readiness snapshot for the next stage transition.
///
/// Returns `None` at Stage 3 (terminal) or below Lvl.99 when the transition
/// isn't imminent — the UI should surface readiness only when it is
/// actionable. Stage 1→2 and Stage 2→3 re-use the same gates
/// [`check_stage1_gates`]/[`check_stage2_gates`] as the authoritative
/// transition path, but return per-gate gaps instead of a single bool.
pub fn compute_readiness(
    evo: &EvolutionState,
    vitals: &VitalsState,
    bond: &BondState,
) -> Option<ReadinessReport> {
    if evo.stage == Stage::Final {
        return None;
    }
    if evo.level < 99 {
        return None;
    }

    let min_vital = vitals
        .stability
        .min(vitals.focus)
        .min(vitals.sync)
        .min(vitals.growth)
        .min(vitals.happiness);

    let mut blocking: Vec<BlockingGate> = Vec::new();
    // Total number of gates considered for this stage — used to compute the
    // coarse progress fraction. Keep in sync with the checks below.
    let gate_count: f32;

    match evo.stage {
        Stage::Base => {
            // 3 gates at Stage 1→2: bond, min_vital, dominance. Level is
            // short-circuited at the function entry.
            gate_count = 3.0;

            if bond.score < 30 {
                blocking.push(BlockingGate {
                    name: "bond".to_string(),
                    current: bond.score as f64,
                    target: 30.0,
                    hint: "Spend more time interacting — bond grows with use.".to_string(),
                });
            }
            if min_vital < 20 {
                blocking.push(BlockingGate {
                    name: "min_vital".to_string(),
                    current: min_vital as f64,
                    target: 20.0,
                    hint: "One of the 5 vitals is below 20 — check `/status`.".to_string(),
                });
            }
            // Dominance ratio: top archetype >= 1.3× runner-up (or runner-up = 0).
            let mut scores: Vec<u32> = evo.archetype_scores.values().copied().collect();
            scores.sort_unstable_by(|a, b| b.cmp(a));
            let top = scores.first().copied().unwrap_or(0);
            let runner = scores.get(1).copied().unwrap_or(0);
            if top == 0 {
                blocking.push(BlockingGate {
                    name: "dominant_archetype".to_string(),
                    current: 0.0,
                    target: 1.0,
                    hint: "No archetype has earned XP yet.".to_string(),
                });
            } else if runner > 0 && (top as f64) < (runner as f64 * 1.3) {
                blocking.push(BlockingGate {
                    name: "dominance_ratio".to_string(),
                    current: (top as f64) / (runner as f64),
                    target: 1.3,
                    hint: "Lean into your top archetype — it needs a 1.3× lead.".to_string(),
                });
            }
        }
        Stage::Evolved => {
            // 3 gates at Stage 2→3: bond, correction_rate, archetype_stable_days.
            // Level is short-circuited at the function entry. The last two
            // gates require DB access (vitals-events correction rate,
            // dominant-shift history) so we surface them as unknown-but-pending
            // BlockingGates here and force `ready = false` — callers that need
            // a true/false answer must go through `check_stage2_gates` with
            // the full DB-backed inputs.
            gate_count = 3.0;

            if bond.score < 55 {
                blocking.push(BlockingGate {
                    name: "bond".to_string(),
                    current: bond.score as f64,
                    target: 55.0,
                    hint: "Deepen the partnership — bond must reach 55.".to_string(),
                });
            }
            blocking.push(BlockingGate {
                name: "correction_rate".to_string(),
                current: f64::NAN,
                target: 0.20,
                hint: "Correction rate must stay below 20% over 14 days.".to_string(),
            });
            blocking.push(BlockingGate {
                name: "archetype_stable_days".to_string(),
                current: f64::NAN,
                target: 14.0,
                hint: "Dominant archetype must hold steady for 14 days.".to_string(),
            });
        }
        Stage::Final => return None,
    }

    let ready = match evo.stage {
        Stage::Base => check_stage1_gates(evo, bond.score, min_vital),
        Stage::Evolved => {
            // Correction-rate / stable-days aren't available here — defer
            // authoritative readiness to `check_stage2_gates` at the call site.
            false
        }
        Stage::Final => false,
    };

    let progress = if gate_count == 0.0 {
        1.0
    } else {
        let cleared = gate_count - blocking.len() as f32;
        (cleared / gate_count).clamp(0.0, 1.0)
    };

    Some(ReadinessReport {
        ready,
        blocking,
        progress,
    })
}

/// Produce 1–3 short, concrete next-step hints tailored to the current state.
pub fn next_step_hints(
    evo: &EvolutionState,
    vitals: &VitalsState,
    bond: &BondState,
) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();

    // Prefer readiness-driven hints when near an evolution.
    if let Some(report) = compute_readiness(evo, vitals, bond) {
        for gate in report.blocking.iter().take(3) {
            hints.push(gate.hint.clone());
        }
        if !hints.is_empty() {
            return hints;
        }
    }

    // Otherwise: steer the user toward their trend.
    if evo.level < 99 {
        let xp_needed = evo.xp_to_next_level;
        hints.push(format!("{} XP to Lvl.{}.", xp_needed, evo.level + 1));
    }

    if let Some(arch) = evo.dominant_archetype {
        hints.push(format!(
            "Dominant archetype: {}. Aligned tool calls earn bonus XP.",
            capitalize(&arch.to_string())
        ));
    } else {
        hints.push("Use tools to start forming an archetype identity.".to_string());
    }

    let min_vital = vitals
        .stability
        .min(vitals.focus)
        .min(vitals.sync)
        .min(vitals.growth)
        .min(vitals.happiness);
    if min_vital < 40 {
        hints.push("Lowest vital is below 40 — check `/status`.".to_string());
    }

    hints.truncate(3);
    hints
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ev(event_type: &str, archetype: Option<&str>, xp: i32, ts: i64) -> EvolutionEvent {
        EvolutionEvent {
            id: 0,
            event_type: event_type.to_string(),
            xp_delta: xp,
            archetype: archetype.map(String::from),
            source: "t".to_string(),
            metadata_json: None,
            created_at: ts,
            hmac: String::new(),
            prev_hmac: String::new(),
        }
    }

    #[test]
    fn render_bar_boundary_values() {
        assert_eq!(render_bar(0, 100, 10), "░".repeat(10));
        assert_eq!(render_bar(100, 100, 10), "█".repeat(10));
        assert_eq!(
            render_bar(50, 100, 10),
            format!("{}{}", "█".repeat(5), "░".repeat(5))
        );
    }

    #[test]
    fn render_bar_clamps_overflow() {
        assert_eq!(render_bar(200, 100, 10), "█".repeat(10));
    }

    #[test]
    fn render_bar_zero_max_treats_as_one() {
        // max=0 -> treat as 1 to avoid division by zero; val=0 => all empty.
        assert_eq!(render_bar(0, 0, 5), "░".repeat(5));
    }

    #[test]
    fn render_bar_zero_width_is_empty() {
        assert_eq!(render_bar(10, 100, 0), "");
    }

    #[test]
    fn momentum_rising_archetype() {
        let now = 1_000_000i64;
        let recent = now - 86_400;
        let prior = now - 10 * 86_400;
        let events = vec![
            ev("xp_gain", Some("ops"), 2, recent),
            ev("xp_gain", Some("ops"), 2, recent),
            ev("xp_gain", Some("ops"), 2, recent),
            ev("xp_gain", Some("ops"), 2, recent),
            ev("xp_gain", Some("ops"), 2, prior),
        ];
        let trends = compute_momentum(&events, now);
        assert_eq!(trends.get(&Archetype::Ops), Some(&Trend::Rising));
    }

    #[test]
    fn momentum_falling_archetype() {
        let now = 1_000_000i64;
        let recent = now - 86_400;
        let prior = now - 10 * 86_400;
        let events = vec![
            ev("xp_gain", Some("builder"), 2, recent),
            ev("xp_gain", Some("builder"), 2, prior),
            ev("xp_gain", Some("builder"), 2, prior),
            ev("xp_gain", Some("builder"), 2, prior),
            ev("xp_gain", Some("builder"), 2, prior),
        ];
        let trends = compute_momentum(&events, now);
        assert_eq!(trends.get(&Archetype::Builder), Some(&Trend::Falling));
    }

    #[test]
    fn momentum_omits_never_seen_archetype() {
        let events: Vec<EvolutionEvent> = vec![];
        let trends = compute_momentum(&events, 1_000_000);
        assert!(trends.is_empty());
    }

    fn mk_evo_state(stage: Stage, level: u8) -> EvolutionState {
        let mut scores: HashMap<Archetype, u32> = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        EvolutionState {
            stage,
            level,
            total_xp: 0,
            xp_to_next_level: 0,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: None,
            evolution_description: None,
            archetype_scores: scores.clone(),
            lifetime_scores: scores.clone(),
            last_30d_scores: scores,
            dominant_history: vec![(0, Archetype::Ops)],
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    fn mk_vitals(v: u8) -> VitalsState {
        use chrono::Utc;
        VitalsState {
            stability: v,
            focus: v,
            sync: v,
            growth: v,
            happiness: v,
            last_interaction_at: Utc::now(),
            updated_at: Utc::now(),
            chain_valid: true,
        }
    }

    fn mk_bond(score: u8) -> BondState {
        use crate::bond::{AutonomyTier, BondLevel};
        BondState {
            score,
            level: BondLevel::Fragile,
            autonomy_tier: AutonomyTier::ObserveOnly,
            total_events: 0,
            chain_valid: true,
        }
    }

    #[test]
    fn readiness_none_at_final_stage() {
        let evo = mk_evo_state(Stage::Final, 99);
        let vitals = mk_vitals(100);
        let bond = mk_bond(100);
        assert!(compute_readiness(&evo, &vitals, &bond).is_none());
    }

    #[test]
    fn readiness_none_below_level_99() {
        let evo = mk_evo_state(Stage::Base, 50);
        let vitals = mk_vitals(100);
        let bond = mk_bond(100);
        assert!(compute_readiness(&evo, &vitals, &bond).is_none());
    }

    #[test]
    fn readiness_stage1_reports_bond_gap() {
        let evo = mk_evo_state(Stage::Base, 99);
        let vitals = mk_vitals(100);
        let bond = mk_bond(10); // below 30
        let report = compute_readiness(&evo, &vitals, &bond).expect("some");
        assert!(!report.ready);
        assert!(report
            .blocking
            .iter()
            .any(|g| g.name == "bond" && g.target == 30.0));
    }

    #[test]
    fn mood_ascending_at_lvl99_near_evolution() {
        let evo = mk_evo_state(Stage::Base, 99);
        let vitals = mk_vitals(80);
        let bond = mk_bond(50);
        assert_eq!(compute_mood(&evo, &vitals, &bond), Mood::Ascending);
    }

    #[test]
    fn mood_strained_when_vital_low() {
        let evo = mk_evo_state(Stage::Base, 40);
        let vitals = mk_vitals(10);
        let bond = mk_bond(50);
        assert_eq!(compute_mood(&evo, &vitals, &bond), Mood::Strained);
    }
}
