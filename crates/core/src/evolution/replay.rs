//! Event replay (event sourcing) for evolution state computation.

use std::collections::HashMap;

use crate::hmac_chain;

use super::{
    dominant_archetype, evolution_gates_verified, level_from_xp, rate_limit_for, verify_event_hmac,
    Archetype, EvolutionEvent, EvolutionState, Stage,
};

#[cfg(test)]
use super::EVOLUTION_HMAC_LEGACY;

/// Replay verified events from baseline to compute current evolution state.
/// Verifies HMAC chain and applies rate limits per event type per hour.
#[cfg(test)]
pub fn replay_events(events: &[EvolutionEvent]) -> EvolutionState {
    replay_events_with_key(EVOLUTION_HMAC_LEGACY, events)
}

/// Replay events with a specific HMAC key (for per-installation derived keys).
pub fn replay_events_with_key(key: &[u8], events: &[EvolutionEvent]) -> EvolutionState {
    let mut stage = Stage::Base;
    let mut total_xp: u32 = 0;
    let mut archetype_scores: HashMap<Archetype, u32> = HashMap::new();
    let mut evolution_name: Option<String> = None;
    let mut evolution_description: Option<String> = None;
    let mut chain_valid = true;
    let mut expected_prev_hmac = "0".to_string();
    let mut accepted_events: u32 = 0;
    let mut rate_limiter = hmac_chain::HourlyRateLimiter::new(None, None);

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

        match event.event_type.as_str() {
            "xp_gain" => {
                // Diminishing returns: repeated same-source calls yield less XP
                let source_multiplier =
                    rate_limiter.source_decay_multiplier(event.created_at, &event.source);
                let effective_xp =
                    (event.xp_delta.max(0) as f64 * source_multiplier).floor() as u32;
                total_xp = total_xp.saturating_add(effective_xp);
                // Update archetype score with decayed XP
                if let Some(ref arch_str) = event.archetype {
                    if let Some(arch) = Archetype::parse(arch_str) {
                        let score = archetype_scores.entry(arch).or_insert(0);
                        *score = score.saturating_add(effective_xp);
                    }
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
                // Extract name and description from metadata
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
            "classification" | "archetype_shift" => {
                // Informational — metadata may update dominant archetype tracking
            }
            _ => {}
        }
    }

    let (level, xp_to_next) = level_from_xp(&stage, total_xp);

    EvolutionState {
        stage,
        level,
        total_xp,
        xp_to_next_level: xp_to_next,
        dominant_archetype: dominant_archetype(&archetype_scores),
        evolution_name,
        evolution_description,
        archetype_scores,
        total_events: accepted_events,
        chain_valid,
    }
}
