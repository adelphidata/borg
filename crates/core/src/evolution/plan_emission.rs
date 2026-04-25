//! Plan-mode emission XP rewards.
//!
//! Plan mode previously earned ~⅓ the XP rate of Execute mode because the
//! creation events that drive the +2/+3 awards (apply_patch, write_memory)
//! are blocked. This module awards a per-session creation-tier XP grant when
//! the agent emits a `<proposed_plan>` block in Plan mode, classifying the
//! plan text against the archetype keyword tables.
//!
//! Per-session cap: at most one `plan_emission` event per session — a long
//! conversation that revises its plan repeatedly cannot farm XP.

use anyhow::Result;

use super::{classify_plan_content, Archetype};
use crate::db::Database;

/// Base XP for emitting a plan in Plan mode.
const BASE_XP_PLAN: i32 = 2;
/// Bonus XP when the plan's archetype matches the current dominant archetype.
const BONUS_XP_PLAN_ALIGNED: i32 = 1;

/// Record a plan-emission XP grant for the given session.
///
/// Returns `Ok(true)` if a new event was inserted, `Ok(false)` if a
/// `plan_emission` event already exists for this `session_id` (idempotent —
/// repeated plan revisions in the same session are ignored).
///
/// The plan text is classified via [`classify_plan_content`]; if no
/// archetype matches, the XP is still awarded against the base tier so plan
/// content the keyword tables don't recognize doesn't silently drop reward.
pub fn record_plan_emission(db: &Database, session_id: &str, plan_text: &str) -> Result<bool> {
    if already_emitted(db, session_id)? {
        return Ok(false);
    }

    let archetype = classify_plan_content(plan_text);
    let dominant = match db.get_evolution_state() {
        Ok(s) => s.dominant_archetype,
        Err(e) => {
            tracing::warn!("plan_emission: get_evolution_state failed: {e}");
            None
        }
    };

    let aligned = matches!((archetype, dominant), (Some(a), Some(b)) if a == b);
    let xp = BASE_XP_PLAN + if aligned { BONUS_XP_PLAN_ALIGNED } else { 0 };

    let metadata = build_metadata(session_id, archetype);
    db.record_evolution_event(
        "xp_gain",
        xp,
        archetype.map(|a| a.to_string()).as_deref(),
        "plan_emission",
        Some(&metadata),
    )?;
    Ok(true)
}

fn already_emitted(db: &Database, session_id: &str) -> Result<bool> {
    let conn = db.conn();
    let existing: i64 = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM evolution_events
             WHERE source = 'plan_emission'
               AND json_extract(metadata_json, '$.session_id') = ?1
         )",
        rusqlite::params![session_id],
        |row| row.get(0),
    )?;
    Ok(existing != 0)
}

fn build_metadata(session_id: &str, archetype: Option<Archetype>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "session_id".to_string(),
        serde_json::Value::String(session_id.to_string()),
    );
    if let Some(a) = archetype {
        obj.insert(
            "archetype".to_string(),
            serde_json::Value::String(a.to_string()),
        );
    }
    serde_json::Value::Object(obj).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::Archetype;

    #[test]
    fn classify_plan_content_table() {
        let cases: &[(&str, Option<Archetype>)] = &[
            (
                "Step 1: deploy kubernetes manifest with kubectl apply",
                Some(Archetype::Ops),
            ),
            (
                "Draft a campaign funnel with seo landing pages",
                Some(Archetype::Marketer),
            ),
            (
                "Wire up Stripe invoice handling and the checkout flow",
                Some(Archetype::Merchant),
            ),
            ("just a plain note about nothing in particular", None),
        ];
        for (text, expected) in cases {
            assert_eq!(classify_plan_content(text), *expected, "input: {text:?}");
        }
    }

    #[test]
    fn first_emission_inserts_second_is_skipped() {
        let db = Database::test_db();
        let session_id = "sess-abc";
        let plan_text = "Plan: deploy kubernetes via helm";

        let inserted = record_plan_emission(&db, session_id, plan_text).unwrap();
        assert!(inserted, "first plan emission should insert");

        let again = record_plan_emission(&db, session_id, plan_text).unwrap();
        assert!(
            !again,
            "second plan emission for same session must be a no-op"
        );

        // Different session is independent.
        let other = record_plan_emission(&db, "sess-xyz", plan_text).unwrap();
        assert!(other, "different session_id should insert independently");
    }

    #[test]
    fn awards_xp_with_archetype() {
        let db = Database::test_db();
        record_plan_emission(&db, "sess-1", "deploy kubernetes pod").unwrap();
        let events = db.load_all_evolution_events().unwrap();
        let event = events
            .iter()
            .find(|e| e.source == "plan_emission")
            .expect("plan_emission row must exist");
        assert_eq!(event.event_type, "xp_gain");
        assert_eq!(event.archetype.as_deref(), Some("ops"));
        // No prior dominant archetype on a fresh DB → base XP only, no aligned bonus.
        assert_eq!(event.xp_delta, BASE_XP_PLAN);
    }
}
