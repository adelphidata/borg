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

pub(crate) async fn handle(
    args: &CreateGoalArgs,
    db: &Database,
    session_id: &str,
    default_budget: Option<i64>,
    max_budget: Option<i64>,
) -> Result<String> {
    let objective = match validate_objective(&args.objective) {
        Ok(o) => o,
        Err(e) => return Ok(json!({"error": e}).to_string()),
    };
    // Fall back to the config default when the model didn't specify a budget,
    // then to the configured ceiling — a goal with no budget at all is
    // unbounded, which is exactly what the ceiling exists to prevent.
    // `validate_budget` still gets to reject zero/negative defaults before
    // they hit the DB.
    //
    // The fallback is clamped, not validated: a `goals.default_token_budget`
    // left above a later-lowered ceiling would otherwise reject every goal
    // over a number the user never typed. A budget the *model* asked for is
    // still refused outright — that is the limit doing its job.
    let requested = match args.token_budget {
        Some(n) => Some(n),
        None => default_budget
            .map(|d| match max_budget {
                Some(max) => d.min(max),
                None => d,
            })
            .or(max_budget),
    };
    let budget = match validate_budget(requested, max_budget) {
        Ok(b) => b,
        Err(e) => return Ok(json!({"error": e}).to_string()),
    };
    match db
        .insert_goal(session_id, &objective, GoalStatus::Active, budget)
        .await?
    {
        Some(g) => {
            crate::goals::metrics::record_created(&g);
            Ok(json!({
                "ok": true,
                "goal_id": g.goal_id,
                "objective": g.objective,
                "status": g.status.as_str(),
                "token_budget": g.token_budget,
            })
            .to_string())
        }
        None => Ok(json!({
            "error": "a goal already exists for this session; use the user-facing /goal command to edit or clear it"
        })
        .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::GoalStatus;

    async fn db_with_session(id: &str) -> Database {
        let db = Database::test_db().await;
        db.upsert_session(id, 0, 0, 0, "m", "t").await.unwrap();
        db
    }

    #[tokio::test]
    async fn creates_goal_when_none_exists() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "do the thing".into(),
                token_budget: Some(1000),
            },
            &db,
            "s",
            None,
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["objective"], "do the thing");
        assert_eq!(v["status"], "active");
        assert_eq!(v["token_budget"], 1000);
    }

    #[tokio::test]
    async fn rejects_when_goal_already_exists() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "first", GoalStatus::Active, None)
            .await
            .unwrap();
        let out = handle(
            &CreateGoalArgs {
                objective: "second".into(),
                token_budget: None,
            },
            &db,
            "s",
            None,
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("already exists"));
        // First goal is still intact.
        assert_eq!(db.get_goal("s").await.unwrap().unwrap().objective, "first");
    }

    #[tokio::test]
    async fn rejects_empty_objective() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "   ".into(),
                token_budget: None,
            },
            &db,
            "s",
            None,
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("empty"));
        assert!(db.get_goal("s").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn default_budget_applies_when_args_omit_one() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: None,
            },
            &db,
            "s",
            Some(7777),
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["token_budget"], 7777);
    }

    #[tokio::test]
    async fn args_budget_overrides_default() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: Some(500),
            },
            &db,
            "s",
            Some(7777),
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token_budget"], 500);
    }

    #[tokio::test]
    async fn rejects_non_positive_budget() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: Some(0),
            },
            &db,
            "s",
            None,
            None,
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("positive"));
    }

    /// Spec: a `goals.default_token_budget` left above the ceiling must clamp,
    /// not reject. The user never typed the default at goal-creation time, so
    /// refusing every goal over it is an error they cannot act on from the
    /// message — while a budget the *model* chose is still refused outright.
    #[tokio::test]
    async fn a_default_budget_above_the_ceiling_clamps_instead_of_failing() {
        let db = db_with_session("clamp").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "ship it".to_string(),
                token_budget: None,
            },
            &db,
            "clamp",
            Some(50_000),
            Some(10_000),
        )
        .await
        .expect("handler must not error");
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert!(
            v.get("error").is_none(),
            "an over-ceiling default must not block goal creation: {v}"
        );
        assert_eq!(v["token_budget"], 10_000);
    }

    /// Spec: a model that asks for a budget above the configured ceiling must
    /// be refused, and no goal may be written — a rejected budget that still
    /// created an unbounded goal would be worse than no ceiling at all.
    #[tokio::test]
    async fn rejects_budget_above_the_configured_maximum() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: Some(50_000),
            },
            &db,
            "s",
            None,
            Some(10_000),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("exceeds"), "{v:?}");
        assert!(
            db.get_goal("s").await.unwrap().is_none(),
            "a rejected budget must not leave a goal behind"
        );
    }

    /// Spec: with a ceiling set, a goal created with no budget at all inherits
    /// it. Leaving it unbounded is the exact case the ceiling exists to stop.
    #[tokio::test]
    async fn an_unspecified_budget_falls_back_to_the_maximum() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: None,
            },
            &db,
            "s",
            None,
            Some(10_000),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token_budget"], 10_000);
    }

    /// The configured default still wins over the ceiling when both are set —
    /// the ceiling is a limit, not a target.
    #[tokio::test]
    async fn the_default_budget_wins_over_the_maximum() {
        let db = db_with_session("s").await;
        let out = handle(
            &CreateGoalArgs {
                objective: "x".into(),
                token_budget: None,
            },
            &db,
            "s",
            Some(2_000),
            Some(10_000),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token_budget"], 2_000);
    }
}
