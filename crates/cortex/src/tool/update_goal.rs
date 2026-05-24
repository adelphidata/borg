//! `update_goal` tool: model-side hook to mark the goal complete or report a
//! repeated impasse via `status="blocked"`. Pause, resume, budget edits, and
//! objective replacement remain user-driven and never reachable from here.

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

pub(crate) async fn handle(
    args: &UpdateGoalArgs,
    db: &Database,
    session_id: &str,
) -> Result<String> {
    let target = match args.status.as_str() {
        "complete" => GoalStatus::Complete,
        "blocked" => GoalStatus::Blocked,
        other => {
            return Ok(json!({
                "error": format!(
                    "status must be \"complete\" or \"blocked\" (got {other:?}); pause/resume are user-controlled"
                )
            })
            .to_string());
        }
    };
    let current = match db.get_goal(session_id).await? {
        Some(g) => g,
        None => {
            return Ok(json!({
                "error": "no goal is set for this session"
            })
            .to_string());
        }
    };
    if current.status == GoalStatus::Complete {
        // Complete is terminal — never silently overwrite with `blocked`.
        return Ok(json!({
            "ok": true,
            "already_complete": true,
            "goal_id": current.goal_id,
        })
        .to_string());
    }
    if target == GoalStatus::Blocked && current.status == GoalStatus::Blocked {
        return Ok(json!({
            "ok": true,
            "already_blocked": true,
            "goal_id": current.goal_id,
        })
        .to_string());
    }
    let updated = db
        .update_goal(
            session_id,
            GoalUpdate {
                status: Some(target),
                expected_goal_id: Some(current.goal_id),
                ..Default::default()
            },
        )
        .await?;
    match updated {
        Some(g) => {
            match g.status {
                GoalStatus::Complete => crate::goals::metrics::record_completed(&g),
                GoalStatus::Blocked => crate::goals::metrics::record_blocked(&g),
                _ => {}
            }
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
            "error": format!("goal changed concurrently; refusing to set {}", target.as_str())
        })
        .to_string()),
    }
}

#[cfg(test)]
mod accept_status_tests {
    //! Argument-shape spec for the `update_goal` tool: only `"complete"` and
    //! `"blocked"` may be passed; everything else returns an error with no
    //! DB write attempted. Guards the contract documented in the tool JSON
    //! schema so a future schema change can't silently widen the surface.
    use super::*;

    #[derive(serde::Deserialize)]
    struct ErrShape {
        error: String,
    }

    fn parse_err(s: &str) -> Option<String> {
        serde_json::from_str::<ErrShape>(s).ok().map(|e| e.error)
    }

    #[test]
    fn unknown_status_rejected_without_mutating_goal_row() {
        // Spec: any status string outside the {"complete","blocked"} set
        // returns an error and leaves the underlying goal row untouched.
        // The error body must name both accepted values so the model can
        // self-correct without a follow-up tool call.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = crate::db::Database::test_db().await;
            // Seed an Active goal so we can verify it is NOT mutated.
            db.upsert_session("s", 0, 0, 0, "m", "t").await.unwrap();
            db.insert_goal("s", "obj", GoalStatus::Active, None)
                .await
                .unwrap();
            let before = db.get_goal("s").await.unwrap().unwrap();
            for bad in ["paused", "active", "budget_limited", "garbage", ""] {
                let out = handle(
                    &UpdateGoalArgs {
                        status: bad.to_string(),
                    },
                    &db,
                    "s",
                )
                .await
                .expect("handle returns Ok with JSON error body");
                let err = parse_err(&out).unwrap_or_else(|| {
                    panic!("status={bad:?} did not return an error body: {out}")
                });
                assert!(
                    err.contains("complete") && err.contains("blocked"),
                    "error for status={bad:?} should name the two accepted values; got {err:?}"
                );
            }
            // Production-path assertion: row is byte-for-byte unchanged.
            let after = db.get_goal("s").await.unwrap().unwrap();
            assert_eq!(
                before, after,
                "rejected status must not mutate the goal row"
            );
        });
    }
}

#[cfg(test)]
mod ported_tests {
    use super::*;

    async fn db_with_session(id: &str) -> Database {
        let db = Database::test_db().await;
        db.upsert_session(id, 0, 0, 0, "m", "t").await.unwrap();
        db
    }

    #[tokio::test]
    async fn marks_active_goal_complete() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "ship", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        db.account_goal_usage("s", 250, 5, None).await.unwrap();
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "complete");
        assert_eq!(v["tokens_used"], 250);
        assert_eq!(
            db.get_goal("s").await.unwrap().unwrap().status,
            GoalStatus::Complete
        );
    }

    #[tokio::test]
    async fn errors_when_no_goal() {
        let db = db_with_session("s").await;
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("no goal"));
    }

    #[tokio::test]
    async fn idempotent_when_already_complete() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        let _ = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .await
        .unwrap();
        // Second call must not error.
        let out = handle(
            &UpdateGoalArgs {
                status: "complete".into(),
            },
            &db,
            "s",
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["already_complete"], true);
    }
}
