//! `get_goal` tool: returns the current per-session goal as JSON, or `null`.
//!
//! Read-only; allowed in Plan mode.

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::db::Database;

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct GetGoalArgs {}

pub(crate) async fn handle(
    _args: &GetGoalArgs,
    db: &Database,
    session_id: &str,
) -> Result<String> {
    let goal = db.get_goal(session_id).await?;
    let payload = match goal {
        Some(g) => {
            let remaining = g
                .token_budget
                .map(|b| (b - g.tokens_used).max(0))
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null);
            json!({
                "goal_id": g.goal_id,
                "objective": g.objective,
                "status": g.status.as_str(),
                "token_budget": g.token_budget,
                "tokens_used": g.tokens_used,
                "tokens_remaining": remaining,
                "time_used_seconds": g.time_used_seconds,
                "created_at": g.created_at,
                "updated_at": g.updated_at,
            })
        }
        None => serde_json::Value::Null,
    };
    Ok(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::GoalStatus;

    fn db_with_session(id: &str) -> Database {
        let db = Database::test_db();
        db.upsert_session(id, 0, 0, 0, "m", "t").unwrap();
        db
    }

    #[test]
    fn returns_null_when_no_goal() {
        let db = db_with_session("s");
        let out = handle(&GetGoalArgs::default(), &db, "s").unwrap();
        assert_eq!(out, "null");
    }

    #[test]
    fn returns_serialized_goal_with_remaining_budget() {
        let db = db_with_session("s");
        db.insert_goal("s", "ship", GoalStatus::Active, Some(1000))
            .unwrap();
        db.account_goal_usage("s", 300, 5, None).unwrap();

        let out = handle(&GetGoalArgs::default(), &db, "s").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["objective"], "ship");
        assert_eq!(v["status"], "active");
        assert_eq!(v["token_budget"], 1000);
        assert_eq!(v["tokens_used"], 300);
        assert_eq!(v["tokens_remaining"], 700);
    }

    #[test]
    fn tokens_remaining_null_when_unbounded() {
        let db = db_with_session("s");
        db.insert_goal("s", "ship", GoalStatus::Active, None)
            .unwrap();
        let out = handle(&GetGoalArgs::default(), &db, "s").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["tokens_remaining"].is_null());
        assert!(v["token_budget"].is_null());
    }
}
