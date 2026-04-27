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

/// Level thresholds that emit milestones up to (and including) Lvl.99.
/// Past Lvl.99 in Final stage, milestones are generated dynamically every
/// `POST_99_MILESTONE_STRIDE` levels — see `post_99_thresholds_crossed`.
const LEVEL_THRESHOLDS: &[u8] = &[10, 25, 50, 75, 99];

/// In Final stage past Lvl.99, fire a milestone every N levels so progression
/// has rhythm rather than a slow number tick.
const POST_99_MILESTONE_STRIDE: u8 = 25;

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

        // Post-99 milestones in Final stage: every POST_99_MILESTONE_STRIDE
        // levels past 99 (Lvl.125, 150, 175, …). Final has no level cap so
        // long-term users keep getting celebrations.
        if next.stage == Stage::Final {
            for threshold in post_99_thresholds_crossed(prev.level, next.level) {
                let id = format!("level_{threshold}_final");
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

/// Yield every post-99 milestone level (Lvl.125, 150, 175, …) crossed by a
/// level transition `prev_level → next_level`. Returns an empty iterator
/// when no stride boundary lies in the open-closed interval `(prev, next]`.
///
/// Inputs are `u8` but we widen to `u16` internally so a long jump that
/// would otherwise saturate the threshold past `u8::MAX` (255) doesn't loop
/// forever at the level cap.
fn post_99_thresholds_crossed(prev_level: u8, next_level: u8) -> Vec<u8> {
    // Milestones are multiples of POST_99_MILESTONE_STRIDE starting at 125
    // (= 99 + stride + 1 rounded up to the next stride multiple). Lvl.100
    // is *not* a milestone — it's the entry boundary for the post-99
    // piecewise XP curve, not a celebration tick.
    if next_level <= 99 {
        return Vec::new();
    }
    let stride = POST_99_MILESTONE_STRIDE as u16;
    let first: u16 = ((100 / stride) + 1) * stride; // 125 for stride=25
    let prev = prev_level as u16;
    let next = next_level as u16;
    let mut thresholds = Vec::new();
    let mut t = first;
    while t <= next && t <= u8::MAX as u16 {
        if t > prev {
            thresholds.push(t as u8);
        }
        t += stride;
    }
    thresholds
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
            session_id: None,
            pubkey_id: None,
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
            session_id: None,
            pubkey_id: None,
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
    fn level_thresholds_cross_multiple_in_one_call() {
        // A single XP burst (e.g. from a batched tool run) can cross several
        // level thresholds at once. `check_milestones` must emit a milestone
        // per threshold, in threshold order.
        let prev = state(Stage::Base, 9, None);
        let next = state(Stage::Base, 26, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert!(
            ids.contains(&"level_10_base"),
            "missing level_10_base in {ids:?}"
        );
        assert!(
            ids.contains(&"level_25_base"),
            "missing level_25_base in {ids:?}"
        );
        // Thresholds walked in ascending order.
        let pos_10 = ids.iter().position(|id| *id == "level_10_base").unwrap();
        let pos_25 = ids.iter().position(|id| *id == "level_25_base").unwrap();
        assert!(pos_10 < pos_25, "expected level_10 before level_25");
    }

    #[test]
    fn aligned_streak_requires_today_xp() {
        // Seven consecutive days ending yesterday — today (bucket 0) has no
        // aligned XP. The streak check walks backwards from `now`, so today's
        // miss must block the milestone.
        let next = state(Stage::Base, 10, Some(Archetype::Ops));
        let now = 10 * 86_400;
        let mut events = Vec::new();
        for d in 1..=7 {
            events.push(xp(Archetype::Ops, now - d * 86_400));
        }
        let out = check_milestones(&state(Stage::Base, 10, None), &next, 0, 0, &events, now);
        assert!(
            out.iter().all(|m| m.id != "aligned_streak_7d"),
            "streak should not fire without today's aligned XP; got {out:?}"
        );
    }

    #[test]
    fn post_99_milestone_fires_at_125_in_final() {
        let prev = state(Stage::Final, 124, None);
        let next = state(Stage::Final, 125, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert!(
            out.iter().any(|m| m.id == "level_125_final"),
            "expected level_125_final, got {out:?}",
        );
    }

    #[test]
    fn post_99_milestone_does_not_fire_off_stride() {
        // Crossing Lvl.99→100 is not a milestone — only multiples of 25 past 99.
        let prev = state(Stage::Final, 99, None);
        let next = state(Stage::Final, 100, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert!(
            out.iter().all(|m| !m.id.starts_with("level_") || m.id == "level_99_final"
                /* not crossing the 99 boundary either, but excluded for clarity */),
            "unexpected post-99 milestone at Lvl.100: {out:?}",
        );
    }

    #[test]
    fn post_99_milestones_only_in_final_stage() {
        // Base/Evolved cap at 99 so they should never see post-99 milestones
        // even if the call signature theoretically allows it.
        let prev = state(Stage::Evolved, 99, None);
        let next = state(Stage::Evolved, 125, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        assert!(out.iter().all(|m| m.id != "level_125_final"));
    }

    #[test]
    fn post_99_milestones_emit_multiple_on_long_jump() {
        // A user replaying a backlog of XP could cross several post-99
        // strides in one call. Each must fire.
        let prev = state(Stage::Final, 100, None);
        let next = state(Stage::Final, 175, None);
        let out = check_milestones(&prev, &next, 0, 0, &[], 0);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"level_125_final"), "{ids:?}");
        assert!(ids.contains(&"level_150_final"), "{ids:?}");
        assert!(ids.contains(&"level_175_final"), "{ids:?}");
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
