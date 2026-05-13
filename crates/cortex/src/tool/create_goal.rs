//! `create_goal` tool: create a new active goal when none exists.
//!
//! Fails (with an error string the model sees) if a goal already exists.

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::db::Database;
use crate::goals::validate_budget;
use crate::goals::validate_objective;
use crate::goals::GoalStatus;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateGoalArgs {
    pub objective: String,
    #[serde(default)]
    pub token_budget: Option<i64>,
}

pub(crate) fn handle(args: &CreateGoalArgs, db: &Database, session_id: &str) -> Result<String> {
    let objective = match validate_objective(&args.objective) {
        Ok(o) => o,
        Err(e) => return Ok(json!({"error": e}).to_string()),
    };
    let budget = match validate_budget(args.token_budget) {
        Ok(b) => b,
        Err(e) => return Ok(json!({"error": e}).to_string()),
    };
    match db.insert_goal(session_id, &objective, GoalStatus::Active, budget)? {
        Some(g) => Ok(json!({
            "ok": true,
            "goal_id": g.goal_id,
            "objective": g.objective,
            "status": g.status.as_str(),
            "token_budget": g.token_budget,
        })
        .to_string()),
        None => Ok(json!({
            "error": "a goal already exists for this session; use the user-facing /goal command to edit or clear it"
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
    fn creates_goal_when_none_exists() {
        let db = db_with_session("s");
        let out = handle(
            &CreateGoalArgs {
                objective: "do the thing".into(),
                token_budget: Some(1000),
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["objective"], "do the thing");
        assert_eq!(v["status"], "active");
        assert_eq!(v["token_budget"], 1000);
    }

    #[test]
    fn rejects_when_goal_already_exists() {
        let db = db_with_session("s");
        db.insert_goal("s", "first", GoalStatus::Active, None)
            .unwrap();
        let out = handle(
            &CreateGoalArgs {
                objective: "second".into(),
                token_budget: None,
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("already exists"));
        // First goal is still intact.
        assert_eq!(db.get_goal("s").unwrap().unwrap().objective, "first");
    }

    #[test]
    fn rejects_empty_objective() {
        let db = db_with_session("s");
        let out = handle(
            &CreateGoalArgs {
                objective: "   ".into(),
                token_budget: None,
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("empty"));
        assert!(db.get_goal("s").unwrap().is_none());
    }

    #[test]
    fn rejects_non_positive_budget() {
        let db = db_with_session("s");
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: Some(0),
            },
            &db,
            "s",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("positive"));
    }
}
