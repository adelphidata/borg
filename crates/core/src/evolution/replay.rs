//! Event replay (event sourcing) for evolution state computation.

use std::collections::HashMap;

use crate::hmac_chain;

use super::{
    compute_momentum, evolution_gates_verified, level_from_xp, rate_limit_for, verify_event_hmac,
    Archetype, EvolutionEvent, EvolutionState, Stage,
};

#[cfg(test)]
use super::EVOLUTION_HMAC_LEGACY;

/// 30-day window in seconds used for time-weighted archetype scoring.
pub(crate) const WINDOW_30D_SECS: i64 = 30 * 86_400;

/// Weighting: recent activity (last 30d) steers specialization more than lifetime sum.
/// Matches `docs/evolution.md#archetype-scoring`.
pub(crate) const LIFETIME_WEIGHT: f64 = 0.35;
pub(crate) const LAST_30D_WEIGHT: f64 = 0.65;

/// Replay verified events from baseline to compute current evolution state.
/// Verifies HMAC chain and applies rate limits per event type per hour.
#[cfg(test)]
pub fn replay_events(events: &[EvolutionEvent]) -> EvolutionState {
    replay_events_with_key_at(
        EVOLUTION_HMAC_LEGACY,
        events,
        chrono::Utc::now().timestamp(),
    )
}

/// Replay events with a specific HMAC key. Uses `Utc::now()` for the 30-day window —
/// callers that need determinism (tests) should use [`replay_events_with_key_at`].
pub fn replay_events_with_key(key: &[u8], events: &[EvolutionEvent]) -> EvolutionState {
    replay_events_with_key_at(key, events, chrono::Utc::now().timestamp())
}

/// Replay events with an explicit `now_ts` reference point for time-weighted scoring.
pub fn replay_events_with_key_at(
    key: &[u8],
    events: &[EvolutionEvent],
    now_ts: i64,
) -> EvolutionState {
    let mut stage = Stage::Base;
    let mut total_xp: u32 = 0;
    let mut lifetime_scores: HashMap<Archetype, u32> = HashMap::new();
    let mut last_30d_scores: HashMap<Archetype, u32> = HashMap::new();
    let mut evolution_name: Option<String> = None;
    let mut evolution_description: Option<String> = None;
    let mut chain_valid = true;
    let mut expected_prev_hmac = "0".to_string();
    let mut accepted_events: u32 = 0;
    let mut rate_limiter = hmac_chain::HourlyRateLimiter::new(None, None);
    let mut dominant_history: Vec<(i64, Archetype)> = Vec::new();
    let mut last_dominant: Option<Archetype> = None;
    let cutoff_30d = now_ts - WINDOW_30D_SECS;
    let mut accepted: Vec<EvolutionEvent> = Vec::new();

    for event in events {
        // Verify HMAC chain
        if !verify_event_hmac(key, event, &expected_prev_hmac) {
            tracing::warn!(
                "evolution: broken HMAC chain at event {}, skipping",
                event.id
            );
            chain_valid = false;
            continue;
        }
        // Chain advances for every HMAC-valid row, even if we later reject the
        // event semantically. This preserves the downstream chain linkage.
        expected_prev_hmac = event.hmac.clone();

        // Pre-filter: semantically-invalid "evolution" events (missing or false
        // `gates_verified`) must be dropped *before* they consume a rate-limit
        // slot or get counted as accepted, otherwise a flood of bogus evolution
        // rows could exhaust the 3/hr evolution budget and starve legitimate
        // transitions.
        if event.event_type == "evolution" && !evolution_gates_verified(event) {
            tracing::warn!(
                "evolution: rejecting event {} without gates_verified",
                event.id
            );
            continue;
        }

        // Rate limiting: per-type per hour
        let type_cap = rate_limit_for(&event.event_type);
        if !rate_limiter.check_and_consume(event.created_at, &event.event_type, type_cap, false) {
            continue;
        }

        accepted_events += 1;
        accepted.push(event.clone());

        match event.event_type.as_str() {
            "xp_gain" => {
                // Diminishing returns: repeated same-source calls yield less XP
                let source_multiplier =
                    rate_limiter.source_decay_multiplier(event.created_at, &event.source);
                let effective_xp =
                    (event.xp_delta.max(0) as f64 * source_multiplier).floor() as u32;
                total_xp = total_xp.saturating_add(effective_xp);
                if let Some(ref arch_str) = event.archetype {
                    if let Some(arch) = Archetype::parse(arch_str) {
                        let lt = lifetime_scores.entry(arch).or_insert(0);
                        *lt = lt.saturating_add(effective_xp);
                        if event.created_at >= cutoff_30d {
                            let rec = last_30d_scores.entry(arch).or_insert(0);
                            *rec = rec.saturating_add(effective_xp);
                        }
                    }
                }
                // Track dominant shifts based on current *effective* scores.
                let current_dominant = dominant_from_effective(&lifetime_scores, &last_30d_scores);
                if current_dominant.is_some() && current_dominant != last_dominant {
                    if let Some(arch) = current_dominant {
                        dominant_history.push((event.created_at, arch));
                    }
                    last_dominant = current_dominant;
                }
            }
            "evolution" => {
                // gates_verified was already validated above. Stage transition:
                // reset XP, advance stage.
                stage = match stage {
                    Stage::Base => Stage::Evolved,
                    Stage::Evolved => Stage::Final,
                    Stage::Final => Stage::Final, // already max
                };
                total_xp = 0;
                if let Some(ref meta) = event.metadata_json {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(meta) {
                        if let Some(name) = parsed.get("name").and_then(|v| v.as_str()) {
                            evolution_name = Some(name.to_string());
                        }
                        if let Some(desc) = parsed.get("description").and_then(|v| v.as_str()) {
                            evolution_description = Some(desc.to_string());
                        }
                    }
                }
            }
            "classification" => {
                // LLM-generated (or fallback) naming arrives as a follow-up
                // `classification` event after the stage transition. It may
                // also arrive standalone (source="llm_naming"). Metadata is
                // strict JSON with optional `name` / `description`.
                if let Some(ref meta) = event.metadata_json {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(meta) {
                        if let Some(name) = parsed.get("name").and_then(|v| v.as_str()) {
                            evolution_name = Some(name.to_string());
                        }
                        if let Some(desc) = parsed.get("description").and_then(|v| v.as_str()) {
                            evolution_description = Some(desc.to_string());
                        }
                    }
                }
            }
            "archetype_shift" => {
                // Informational.
            }
            "level_up" | "milestone_unlocked" | "mood_changed" | "share_card_created" => {
                // Informational / feed-only. Consumed by higher-level helpers
                // (e.g. `level_up_events_recent`, mood transition detection).
            }
            _ => {}
        }
    }

    let (level, xp_to_next) = level_from_xp(&stage, total_xp);
    let effective_scores = effective_scores_map(&lifetime_scores, &last_30d_scores);
    let dominant = dominant_from_effective(&lifetime_scores, &last_30d_scores);
    let momentum = compute_momentum(&accepted, now_ts);

    let mut level_up_events_recent: Vec<EvolutionEvent> = accepted
        .iter()
        .filter(|e| e.event_type == "level_up" || e.event_type == "milestone_unlocked")
        .cloned()
        .collect();
    level_up_events_recent.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    level_up_events_recent.truncate(20);

    EvolutionState {
        stage,
        level,
        total_xp,
        xp_to_next_level: xp_to_next,
        dominant_archetype: dominant,
        evolution_name,
        evolution_description,
        archetype_scores: effective_scores,
        lifetime_scores,
        last_30d_scores,
        dominant_history,
        total_events: accepted_events,
        chain_valid,
        momentum,
        level_up_events_recent,
        mood: None,
        readiness: None,
    }
}

/// Combine lifetime and last-30d scores into the weighted effective map (rounded u32).
pub(crate) fn effective_scores_map(
    lifetime: &HashMap<Archetype, u32>,
    last_30d: &HashMap<Archetype, u32>,
) -> HashMap<Archetype, u32> {
    let mut out = HashMap::new();
    for (arch, lt) in lifetime {
        let rec = last_30d.get(arch).copied().unwrap_or(0);
        let eff = (*lt as f64) * LIFETIME_WEIGHT + (rec as f64) * LAST_30D_WEIGHT;
        out.insert(*arch, eff.round() as u32);
    }
    // Include archetypes that appear only in last_30d (possible if lifetime overflowed,
    // though u32::saturating_add makes this unlikely).
    for (arch, rec) in last_30d {
        out.entry(*arch)
            .or_insert_with(|| ((*rec as f64) * LAST_30D_WEIGHT).round() as u32);
    }
    out
}

/// Pick the archetype with the highest effective score. Returns `None` if all scores are 0.
pub(crate) fn dominant_from_effective(
    lifetime: &HashMap<Archetype, u32>,
    last_30d: &HashMap<Archetype, u32>,
) -> Option<Archetype> {
    let effective = effective_scores_map(lifetime, last_30d);
    effective
        .into_iter()
        .filter(|(_, score)| *score > 0)
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)))
        })
        .map(|(arch, _)| arch)
}
