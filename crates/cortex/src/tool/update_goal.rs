//! `update_goal` tool: model-side hook to mark the goal complete.
//!
//! The schema exposes a single accepted value (`status="complete"`). Pause,
//! resume, budget edits, and objective replacement are user-driven and never
//! reachable through this tool.

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::db::Database;
use crate::db::GoalUpdate;
use crate::goals::GoalStatus;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateGoalArgs {
    pub status: String,
}

pub(crate) fn handle(args: &UpdateGoalArgs, db: &Database, session_id: &str) -> Result<String> {
    if args.status != "complete" {
        return Ok(json!({
            "error": format!("status must be \"complete\" (got {:?}); pause/resume are user-controlled", args.status)
        })
        .to_string());
    }
    let current = match db.get_goal(session_id)? {
        Some(g) => g,
        None => {
            return Ok(json!({
                "error": "no goal is set for this session"
            })
            .to_string());
        }
    };
    if current.status == GoalStatus::Complete {
        return Ok(json!({
            "ok": true,
            "already_complete": true,
            "goal_id": current.goal_id,
        })
        .to_string());
    }
    let updated = db.update_goal(
        session_id,
        GoalUpdate {
            status: Some(GoalStatus::Complete),
            expected_goal_id: Some(current.goal_id),
            ..Default::default()
        },
    )?;
    match updated {
        Some(g) => {
            crate::goals::metrics::record_completed(&g);
            Ok(json!({
                "ok": true,
                "goal_id": g.goal_id,
                "status": g.status.as_str(),
                "tokens_used": g.tokens_used,
                "token_budget": g.token_budget,
                "time_used_seconds": g.time_used_seconds,
            })
            .to_string())
        }
        None => Ok(json!({
            "error": "goal changed concurrently; refusing to mark complete"
        })
        .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_session(id: &str) -> Database {
        let db = Database::test_db();
        db.upsert_session(id, 0, 0, 0, "m", "t").unwrap();
        db
    }

    #[test]
    fn rejects_status_values_other_than_complete() {
        let db = db_with_session("s");
        db.insert_goal("s", "x", GoalStatus::Active, None).unwrap();
        for bad in ["paused", "active", "budget_limited", "garbage", ""] {
            let out = handle(
                &UpdateGoalArgs {
                    status: bad.to_string(),
                },
                &db,
                "s",
            )
            .unwrap();
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert!(
                v["error"].as_str().unwrap().contains("must be"),
                "expected error for status={bad:?}, got {out}"
            );
        }
        // Goal still Active.
        assert_eq!(
            db.get_goal("s").unwrap().unwrap().status,
            GoalStatus::Active
        );
    }

    #[test]
    fn marks_active_goal_complete() {
        let db = db_with_session("s");
        db.insert_goal("s", "ship", GoalStatus::Active, Some(1000))
            .unwrap();
        db.account_goal_usage("s", 250, 5, None).unwrap();
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "complete");
        assert_eq!(v["tokens_used"], 250);
        assert_eq!(
            db.get_goal("s").unwrap().unwrap().status,
            GoalStatus::Complete
        );
    }

    #[test]
    fn errors_when_no_goal() {
        let db = db_with_session("s");
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("no goal"));
    }

    #[test]
    fn idempotent_when_already_complete() {
        let db = db_with_session("s");
        db.insert_goal("s", "x", GoalStatus::Active, None).unwrap();
        let _ = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .unwrap();
        // Second call must not error.
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["already_complete"], true);
    }
}
