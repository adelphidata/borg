//! Sub-evolution milestone detection for the V2 surface.
//!
//! Compares pre/post evolution state (plus bond + event history) and returns
//! any newly-unlocked milestones. Callers are responsible for emitting the
//! corresponding `milestone_unlocked` / `level_up` events and celebrations.
//!
//! Dedup lives here too: the `events` slice is scanned for prior
//! `milestone_unlocked` rows so we never re-emit a milestone that already
//! fired.

use std::collections::HashSet;

use super::{Archetype, EvolutionEvent, EvolutionState, Stage};

/// Consecutive-day streak threshold for `aligned_streak_7d`.
const ALIGNED_STREAK_DAYS: i64 = 7;

/// Archetype stability threshold (seconds) for `archetype_stabilized`.
const ARCHETYPE_STABILITY_SECS: i64 = 7 * 86_400;

/// Bond score threshold for `first_strong_bond`.
const FIRST_STRONG_BOND_THRESHOLD: u8 = 55;

/// Level thresholds that emit milestones.
const LEVEL_THRESHOLDS: &[u8] = &[10, 25, 50, 75, 99];

/// A milestone unlocked by the current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Stable identifier, e.g. `"level_10_base"` or `"first_evolution"`.
    pub id: String,
    /// Short user-facing title.
    pub title: String,
    /// Associated archetype, if any.
    pub archetype: Option<Archetype>,
}

/// Compare pre/post evolution state and bond snapshots against the event log,
/// returning any newly unlocked milestones.
///
/// Dedup is performed by scanning `events` for prior `milestone_unlocked`
/// rows; each milestone fires at most once per `id`.
pub fn check_milestones(
    prev: &EvolutionState,
    next: &EvolutionState,
    bond_prev: u8,
    bond_next: u8,
    events: &[EvolutionEvent],
    now: i64,
) -> Vec<Milestone> {
    let already_unlocked = collect_unlocked_ids(events);
    let mut out = Vec::new();

    // Level thresholds — only when stage is unchanged so crossing a level
    // after an evolution (which resets to 0) fires a new row per stage.
    if prev.stage == next.stage {
        let stage_label = stage_label(next.stage);
        for &threshold in LEVEL_THRESHOLDS {
            if prev.level < threshold && next.level >= threshold {
                let id = format!("level_{threshold}_{stage_label}");
                if !already_unlocked.contains(&id) {
                    out.push(Milestone {
                        id,
                        title: format!("Lvl.{threshold}"),
                        archetype: next.dominant_archetype,
                    });
                }
            }
        }
    }

    // Stage::Base → Stage::Evolved (first evolution).
    if prev.stage == Stage::Base
        && next.stage == Stage::Evolved
        && !already_unlocked.contains("first_evolution")
    {
        out.push(Milestone {
            id: "first_evolution".to_string(),
            title: "First Evolution".to_string(),
            archetype: next.dominant_archetype,
        });
    }

    // First strong bond (crossing the 55 threshold upward).
    if bond_prev < FIRST_STRONG_BOND_THRESHOLD
        && bond_next >= FIRST_STRONG_BOND_THRESHOLD
        && !already_unlocked.contains("first_strong_bond")
    {
        out.push(Milestone {
            id: "first_strong_bond".to_string(),
            title: "Strong Bond".to_string(),
            archetype: next.dominant_archetype,
        });
    }

    // Archetype stabilized — last dominant shift aged ≥ 7 days.
    if !already_unlocked.contains("archetype_stabilized") {
        if let Some((shift_ts, arch)) = next.dominant_history.last() {
            if now.saturating_sub(*shift_ts) >= ARCHETYPE_STABILITY_SECS {
                out.push(Milestone {
                    id: "archetype_stabilized".to_string(),
                    title: "Archetype Stabilized".to_string(),
                    archetype: Some(*arch),
                });
            }
        }
    }

    // Aligned streak: 7 consecutive UTC days with ≥1 xp_gain event aligned
    // with the current dominant archetype.
    if !already_unlocked.contains("aligned_streak_7d") {
        if let Some(arch) = next.dominant_archetype {
            if has_aligned_streak(events, arch, now) {
                out.push(Milestone {
                    id: "aligned_streak_7d".to_string(),
                    title: "7-Day Aligned Streak".to_string(),
                    archetype: Some(arch),
                });
            }
        }
    }

    out
}

/// Stage label matching the string form used elsewhere (base/evolved/final).
fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Base => "base",
        Stage::Evolved => "evolved",
        Stage::Final => "final",
    }
}

/// Scan prior `milestone_unlocked` rows and collect their `milestone_id`s.
fn collect_unlocked_ids(events: &[EvolutionEvent]) -> HashSet<String> {
    let mut out = HashSet::new();
    for e in events {
        if e.event_type != "milestone_unlocked" {
            continue;
        }
        let Some(meta) = e.metadata_json.as_deref() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(meta) else {
            continue;
        };
        if let Some(id) = v.get("milestone_id").and_then(|v| v.as_str()) {
            out.insert(id.to_string());
        }
    }
    out
}

/// True if `events` contains at least one aligned `xp_gain` on each of the
/// last 7 UTC calendar days ending with the day containing `now`.
fn has_aligned_streak(events: &[EvolutionEvent], arch: Archetype, now: i64) -> bool {
    let arch_str = arch.to_string();
    let today_bucket = now / 86_400;

    for offset in 0..ALIGNED_STREAK_DAYS {
        let target = today_bucket - offset;
        let matched = events.iter().any(|e| {
            e.event_type == "xp_gain"
                && e.archetype.as_deref() == Some(arch_str.as_str())
                && (e.created_at / 86_400) == target
        });
        if !matched {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn state(stage: Stage, level: u8, arch: Option<Archetype>) -> EvolutionState {
        EvolutionState {
            stage,
            level,
            total_xp: 0,
            xp_to_next_level: 0,
            dominant_archetype: arch,
            evolution_name: None,
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    fn ev(event_type: &str, meta: Option<&str>) -> EvolutionEvent {
        EvolutionEvent {
            id: 0,
            event_type: event_type.to_string(),
            xp_delta: 0,
            archetype: None,
            source: "test".to_string(),
            metadata_json: meta.map(|s| s.to_string()),
            created_at: 0,
            hmac: String::new(),
            prev_hmac: String::new(),
        }
    }

    fn xp(arch: Archetype, created_at: i64) -> EvolutionEvent {
        EvolutionEvent {
            id: 0,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: Some(arch.to_string()),
            source: "test".to_string(),
            metadata_json: None,
            created_at,
            hmac: String::new(),
            prev_hmac: String::new(),
        }
    }

    #[test]
    fn level_threshold_fires_on_crossing() {
        let prev = state(Stage::Base, 9, None);
        let next = state(Stage::Base, 10, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "level_10_base");
    }

    #[test]
    fn level_threshold_includes_stage_in_id() {
        let prev = state(Stage::Evolved, 24, None);
        let next = state(Stage::Evolved, 25, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "level_25_evolved");
    }

    #[test]
    fn level_threshold_suppressed_across_stage_transitions() {
        // Stage transition resets level to 0 — do not fire level milestones.
        let prev = state(Stage::Base, 99, None);
        let next = state(Stage::Evolved, 0, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        // Only `first_evolution` fires here, not a level row.
        assert!(out.iter().all(|m| !m.id.starts_with("level_")));
    }

    #[test]
    fn first_evolution_fires_on_base_to_evolved() {
        let prev = state(Stage::Base, 99, None);
        let next = state(Stage::Evolved, 0, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert!(out.iter().any(|m| m.id == "first_evolution"));
    }

    #[test]
    fn first_strong_bond_fires_once() {
        let prev = state(Stage::Base, 0, None);
        let next = state(Stage::Base, 0, None);
        let out = check_milestones(&prev, &next, 54, 55, &[], 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "first_strong_bond");

        // Now simulate it already being in the event stream.
        let prior = vec![ev(
            "milestone_unlocked",
            Some(r#"{"milestone_id":"first_strong_bond"}"#),
        )];
        let out2 = check_milestones(&prev, &next, 54, 55, &prior, 0);
        assert!(out2.iter().all(|m| m.id != "first_strong_bond"));
    }

    #[test]
    fn archetype_stabilized_requires_7_day_aging() {
        let mut next = state(Stage::Base, 10, Some(Archetype::Ops));
        let now = 10 * 86_400;
        // Last shift 8 days ago → fires.
        next.dominant_history
            .push((now - 8 * 86_400, Archetype::Ops));
        let out = check_milestones(&state(Stage::Base, 10, None), &next, 0, 0, &[], now);
        assert!(out.iter().any(|m| m.id == "archetype_stabilized"));
    }

    #[test]
    fn archetype_stabilized_suppressed_when_recent() {
        let mut next = state(Stage::Base, 10, Some(Archetype::Ops));
        let now = 10 * 86_400;
        next.dominant_history.push((now - 86_400, Archetype::Ops));
        let out = check_milestones(&state(Stage::Base, 10, None), &next, 0, 0, &[], now);
        assert!(out.iter().all(|m| m.id != "archetype_stabilized"));
    }

    #[test]
    fn aligned_streak_requires_7_consecutive_days() {
        let next = state(Stage::Base, 10, Some(Archetype::Ops));
        let now = 10 * 86_400;
        let mut events = Vec::new();
        for d in 0..7 {
            events.push(xp(Archetype::Ops, now - d * 86_400));
        }
        let out = check_milestones(&state(Stage::Base, 10, None), &next, 0, 0, &events, now);
        assert!(out.iter().any(|m| m.id == "aligned_streak_7d"));
    }

    #[test]
    fn aligned_streak_gap_blocks_milestone() {
        let next = state(Stage::Base, 10, Some(Archetype::Ops));
        let now = 10 * 86_400;
        let mut events = Vec::new();
        // Miss day 3.
        for d in 0..7 {
            if d == 3 {
                continue;
            }
            events.push(xp(Archetype::Ops, now - d * 86_400));
        }
        let out = check_milestones(&state(Stage::Base, 10, None), &next, 0, 0, &events, now);
        assert!(out.iter().all(|m| m.id != "aligned_streak_7d"));
    }

    #[test]
    fn idempotent_on_repeated_calls() {
        let prev = state(Stage::Base, 9, None);
        let next = state(Stage::Base, 10, None);
        let first = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert_eq!(first.len(), 1);

        // Feed the emitted milestone back in as an event and check that the
        // second call skips it.
        let meta = format!(r#"{{"milestone_id":"{}"}}"#, first[0].id);
        let events = vec![ev("milestone_unlocked", Some(&meta))];
        let second = check_milestones(&prev, &next, 0, 0, &events, 0);
        assert!(second.iter().all(|m| m.id != "level_10_base"));
    }
}
