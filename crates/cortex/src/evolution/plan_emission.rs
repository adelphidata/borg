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
pub async fn record_plan_emission(
    db: &Database,
    session_id: &str,
    plan_text: &str,
) -> Result<bool> {
    if already_emitted(db, session_id).await? {
        return Ok(false);
    }

    let archetype = classify_plan_content(plan_text);
    let dominant = match db.get_evolution_state().await {
        Ok(s) => s.dominant_archetype,
        Err(e) => {
            tracing::warn!("plan_emission: get_evolution_state failed: {e}");
            None
        }
    };

    let aligned = matches!((archetype, dominant), (Some(a), Some(b)) if a == b);
    let align_bonus = if aligned { BONUS_XP_PLAN_ALIGNED } else { 0 };

    let (state, posture_seq, fallback) = super::security::current_posture_for_xp(db).await;
    let breakdown = super::security::compute_xp_breakdown(
        BASE_XP_PLAN as u32,
        align_bonus as u32,
        state,
        posture_seq,
        fallback,
    );
    let metadata = build_metadata(session_id, archetype, &breakdown, posture_seq);
    db.record_evolution_event_with_session(
        "xp_gain",
        breakdown.final_xp as i32,
        archetype.map(|a| a.to_string()).as_deref(),
        "plan_emission",
        Some(&metadata),
        Some(session_id),
    )
    .await?;
    Ok(true)
}

async fn already_emitted(db: &Database, session_id: &str) -> Result<bool> {
    let conn = db.conn();
    // Prefer the indexed `session_id` column; legacy rows fall back to
    // `json_extract` so dedup still holds across the schema bump.
    let mut rows = conn
        .query(
            "SELECT EXISTS(
             SELECT 1 FROM evolution_events
             WHERE source = 'plan_emission'
               AND (
                 session_id = ?1
                 OR json_extract(metadata_json, '$.session_id') = ?1
               )
         )",
            libsql::params![session_id],
        )
        .await?;
    let existing: i64 = match rows.next().await? {
        Some(row) => row.get::<i64>(0)?,
        None => 0,
    };
    Ok(existing != 0)
}

fn build_metadata(
    session_id: &str,
    archetype: Option<Archetype>,
    breakdown: &super::security::XpBreakdown,
    posture_seq: i64,
) -> String {
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
    obj.insert(
        "xp_breakdown".to_string(),
        serde_json::to_value(breakdown).unwrap_or(serde_json::Value::Null),
    );
    obj.insert(
        "posture_event_seq".to_string(),
        serde_json::Value::Number(posture_seq.into()),
    );
    serde_json::Value::Object(obj).to_string()
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
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

    #[test]
    fn plan_emission_applies_posture_multiplier_to_xp() {
        // Real failure mode: plan emission writes raw XP bypassing the
        // posture multiplier — boosted installs would lose the gameplay
        // reward, and the audit-chain replay would disagree with the
        // stored `xp_delta`.
        let db = Database::test_db();
        let now = chrono::Utc::now().timestamp();
        // Seed Reduced posture so 2 × 0.85 = 1.70 → round_half_up → 2.
        // (Stored final_xp clamps to floor=1 if pre_clamp had been < 1.)
        let seq = db
            .append_audit_event(
                "security_posture_changed",
                r#"{"state":"Reduced"}"#,
                "system",
                now,
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO posture_snapshots
                 (evolution_event_id, taken_at, posture, source, schema_version)
                 VALUES (?1, ?2, 'Reduced', 'ConfigChange', 1)",
                rusqlite::params![seq, now],
            )
            .unwrap();

        record_plan_emission(&db, "sess-reduced", "deploy kubernetes pod").unwrap();
        let events = db.load_all_evolution_events().unwrap();
        let xp = events
            .iter()
            .find(|e| e.source == "plan_emission")
            .expect("plan_emission row");
        // Base 2, no alignment → 2 × 0.85 = 1.7 → round to 2.
        assert_eq!(xp.xp_delta, 2);
        let meta: serde_json::Value =
            serde_json::from_str(xp.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["xp_breakdown"]["state"], "Reduced");
        assert_eq!(meta["posture_event_seq"].as_i64(), Some(seq));
    }

    #[test]
    fn session_id_populates_indexed_column() {
        // The schema includes an indexed `session_id` column. The dedup
        // query must work via that column, not just the json_extract fallback.
        let db = Database::test_db();
        record_plan_emission(&db, "sess-via-column", "deploy kubernetes").unwrap();
        let events = db.load_all_evolution_events().unwrap();
        let event = events
            .iter()
            .find(|e| e.source == "plan_emission")
            .expect("plan_emission row must exist");
        assert_eq!(event.session_id.as_deref(), Some("sess-via-column"));
    }
}
