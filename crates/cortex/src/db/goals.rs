//! SQLite persistence for per-session goals.
//!
//! One row per session (PRIMARY KEY = `session_id`). Status changes flow
//! through here; the atomic budget-limit flip in [`Database::account_goal_usage`]
//! is the single point at which the `budget_limited` state can transition
//! from `active` without an explicit user action.

use anyhow::Result;
use turso::params;

use super::Database;
use crate::goals::Goal;
use crate::goals::GoalStatus;

/// Patch values for an in-place goal update.
#[derive(Debug, Default, Clone)]
pub struct GoalUpdate {
    /// Replace the objective (regenerates `goal_id` and resets accounting).
    pub objective: Option<String>,
    /// Change status (e.g. user paused/resumed).
    pub status: Option<GoalStatus>,
    /// Replace token budget. `Some(None)` clears the budget; `Some(Some(n))`
    /// sets a positive budget; `None` leaves the existing budget unchanged.
    pub token_budget: Option<Option<i64>>,
    /// Optional CAS guard — when `Some`, the update only fires if the
    /// existing row's `goal_id` matches. Used by accounting to detect
    /// mid-flight objective replacements.
    pub expected_goal_id: Option<String>,
}

/// Outcome of [`Database::account_goal_usage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAccountingOutcome {
    /// The row was updated (or transitioned to BudgetLimited). Returned goal
    /// reflects post-update state.
    Updated(Goal),
    /// The row was not updated. Either the goal disappeared, or the
    /// `expected_goal_id` CAS did not match, or the goal is not Active.
    Unchanged(Option<Goal>),
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const SELECT_COLS: &str = "session_id, goal_id, objective, status, token_budget, \
                           tokens_used, time_used_seconds, created_at, updated_at";

/// Map a row from `SELECT_COLS` to a [`Goal`]. Returns an error when the
/// row holds an unknown status string.
fn map_row(row: &turso::Row) -> Result<Goal> {
    let status_raw: String = row.get::<String>(3)?;
    let status = GoalStatus::from_str(&status_raw)
        .ok_or_else(|| anyhow::anyhow!("unknown goal status in DB: {status_raw}"))?;
    Ok(Goal {
        session_id: row.get::<String>(0)?,
        goal_id: row.get::<String>(1)?,
        objective: row.get::<String>(2)?,
        status,
        token_budget: row.get::<Option<i64>>(4)?,
        tokens_used: row.get::<i64>(5)?,
        time_used_seconds: row.get::<i64>(6)?,
        created_at: row.get::<i64>(7)?,
        updated_at: row.get::<i64>(8)?,
    })
}

impl Database {
    /// Fetch the goal for `session_id`. Returns `None` if no goal is set.
    pub async fn get_goal(&self, session_id: &str) -> Result<Option<Goal>> {
        let sql = format!("SELECT {SELECT_COLS} FROM goals WHERE session_id = ?1");
        let mut rows = self.conn.query(&sql, params!(session_id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Create a new goal. Fails-as-`None` (no rows inserted) if a goal already
    /// exists for the session. Caller must check the return value rather than
    /// `?`-propagating a missing return — the contract is "fail if a
    /// goal exists; use update_goal".
    pub async fn insert_goal(
        &self,
        session_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<Option<Goal>> {
        let now = now_unix();
        let goal_id = uuid::Uuid::new_v4().to_string();
        let affected = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO goals
                (session_id, goal_id, objective, status, token_budget,
                 tokens_used, time_used_seconds, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?6)",
                params!(
                    session_id,
                    goal_id,
                    objective,
                    status.as_str(),
                    token_budget,
                    now,
                ),
            )
            .await?;
        if affected == 0 {
            return Ok(None);
        }
        self.get_goal(session_id).await
    }

    /// Upsert: regenerate `goal_id`, replace objective, reset accounting.
    /// Used by `/goal <new objective>` when a goal already exists.
    pub async fn replace_goal(
        &self,
        session_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<Goal> {
        let now = now_unix();
        let goal_id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO goals
                (session_id, goal_id, objective, status, token_budget,
                 tokens_used, time_used_seconds, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                goal_id = ?2,
                objective = ?3,
                status = ?4,
                token_budget = ?5,
                tokens_used = 0,
                time_used_seconds = 0,
                updated_at = ?6",
                params!(
                    session_id,
                    goal_id,
                    objective,
                    status.as_str(),
                    token_budget,
                    now,
                ),
            )
            .await?;
        self.get_goal(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("goal disappeared after replace"))
    }

    /// Apply a [`GoalUpdate`] patch. Returns `Ok(None)` if no goal exists or
    /// the optional CAS guard rejected the update.
    pub async fn update_goal(&self, session_id: &str, patch: GoalUpdate) -> Result<Option<Goal>> {
        let existing = match self.get_goal(session_id).await? {
            Some(g) => g,
            None => return Ok(None),
        };
        if let Some(expected) = &patch.expected_goal_id {
            if expected != &existing.goal_id {
                return Ok(None);
            }
        }
        // Objective edit regenerates goal_id and resets accounting.
        if let Some(new_objective) = patch.objective {
            let status = patch.status.unwrap_or(existing.status);
            let budget = patch.token_budget.unwrap_or(existing.token_budget);
            return Ok(Some(
                self.replace_goal(session_id, &new_objective, status, budget)
                    .await?,
            ));
        }
        let now = now_unix();
        let new_status = patch.status.unwrap_or(existing.status);
        let new_budget = patch.token_budget.unwrap_or(existing.token_budget);
        self.conn
            .execute(
                "UPDATE goals SET status = ?2, token_budget = ?3, updated_at = ?4
             WHERE session_id = ?1",
                params!(session_id, new_status.as_str(), new_budget, now),
            )
            .await?;
        self.get_goal(session_id).await
    }

    /// Delete a goal. Returns `true` if a row was deleted.
    pub async fn delete_goal(&self, session_id: &str) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM goals WHERE session_id = ?1",
                params!(session_id),
            )
            .await?;
        Ok(affected > 0)
    }

    /// Charge `token_delta` + `time_delta_s` against the goal and atomically
    /// flip status to `budget_limited` if the cap is crossed.
    ///
    /// Only affects rows whose status is `active`. Other statuses (Paused,
    /// BudgetLimited, Complete) are no-ops by design.
    ///
    /// If `expected_goal_id` is `Some` and the row's `goal_id` does not match,
    /// the update is skipped (returns `Unchanged`). This is the CAS guard the
    /// runtime uses to ignore accounting for a goal that was replaced
    /// mid-turn.
    pub async fn account_goal_usage(
        &self,
        session_id: &str,
        token_delta: i64,
        time_delta_seconds: i64,
        expected_goal_id: Option<&str>,
    ) -> Result<GoalAccountingOutcome> {
        let token_delta = token_delta.max(0);
        let time_delta_seconds = time_delta_seconds.max(0);
        let now = now_unix();
        // Single SQL UPDATE so the read-modify-write is atomic w.r.t. other
        // SQLite writers in this process.
        let affected = self
            .conn
            .execute(
                "UPDATE goals
                SET tokens_used = tokens_used + ?2,
                    time_used_seconds = time_used_seconds + ?3,
                    status = CASE
                        WHEN token_budget IS NOT NULL
                             AND tokens_used + ?2 >= token_budget
                        THEN 'budget_limited'
                        ELSE status
                    END,
                    updated_at = ?4
              WHERE session_id = ?1
                AND status = 'active'
                AND (?5 IS NULL OR goal_id = ?5)",
                params!(
                    session_id,
                    token_delta,
                    time_delta_seconds,
                    now,
                    expected_goal_id.map(std::string::ToString::to_string),
                ),
            )
            .await?;
        let current = self.get_goal(session_id).await?;
        if affected == 0 {
            Ok(GoalAccountingOutcome::Unchanged(current))
        } else {
            // The UPDATE matched, so the row must exist.
            let goal = current
                .ok_or_else(|| anyhow::anyhow!("goal disappeared after accounting update"))?;
            // Emit the budget-limited metric exactly when this UPDATE
            // performed the transition. The CASE expression above is the
            // sole path to BudgetLimited via accounting, and the row was
            // Active before this call (WHERE status='active'), so seeing
            // BudgetLimited now means the transition happened on this
            // call.
            if goal.status == crate::goals::GoalStatus::BudgetLimited {
                crate::goals::metrics::record_budget_limited(&goal);
            }
            Ok(GoalAccountingOutcome::Updated(goal))
        }
    }

    /// Flip an Active goal to Paused, used by the cancel/interrupt flow.
    /// Returns the post-update goal, or `None` if there is no Active goal.
    pub async fn pause_active_goal(&self, session_id: &str) -> Result<Option<Goal>> {
        let now = now_unix();
        let affected = self
            .conn
            .execute(
                "UPDATE goals SET status = 'paused', updated_at = ?2
             WHERE session_id = ?1 AND status = 'active'",
                params!(session_id, now),
            )
            .await?;
        if affected == 0 {
            return Ok(None);
        }
        self.get_goal(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn db_with_session(id: &str) -> Database {
        let db = Database::test_db().await;
        db.upsert_session(id, 0, 0, 0, "test-model", "test")
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn insert_then_get_roundtrips() {
        let db = db_with_session("s").await;
        let inserted = db
            .insert_goal("s", "do the thing", GoalStatus::Active, Some(1000))
            .await
            .unwrap()
            .expect("insert returns the goal");
        assert_eq!(inserted.objective, "do the thing");
        assert_eq!(inserted.status, GoalStatus::Active);
        assert_eq!(inserted.token_budget, Some(1000));
        assert_eq!(inserted.tokens_used, 0);

        let fetched = db.get_goal("s").await.unwrap().unwrap();
        assert_eq!(fetched.goal_id, inserted.goal_id);
    }

    #[tokio::test]
    async fn get_goal_returns_none_when_missing() {
        let db = db_with_session("s").await;
        assert!(db.get_goal("s").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn insert_goal_returns_none_when_one_exists() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "first", GoalStatus::Active, None)
            .await
            .unwrap();
        let second = db
            .insert_goal("s", "second", GoalStatus::Active, None)
            .await
            .unwrap();
        assert!(second.is_none(), "second insert must not silently replace");
        let current = db.get_goal("s").await.unwrap().unwrap();
        assert_eq!(current.objective, "first");
    }

    #[tokio::test]
    async fn replace_goal_regenerates_goal_id_and_resets_accounting() {
        let db = db_with_session("s").await;
        let first = db
            .insert_goal("s", "first", GoalStatus::Active, Some(500))
            .await
            .unwrap()
            .unwrap();
        db.account_goal_usage("s", 100, 10, None).await.unwrap();

        let replaced = db
            .replace_goal("s", "second", GoalStatus::Active, None)
            .await
            .unwrap();
        assert_ne!(replaced.goal_id, first.goal_id);
        assert_eq!(replaced.objective, "second");
        assert_eq!(replaced.tokens_used, 0);
        assert_eq!(replaced.time_used_seconds, 0);
        assert_eq!(replaced.token_budget, None);
    }

    #[tokio::test]
    async fn account_below_budget_does_not_flip() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        let out = db.account_goal_usage("s", 100, 5, None).await.unwrap();
        match out {
            GoalAccountingOutcome::Updated(g) => {
                assert_eq!(g.tokens_used, 100);
                assert_eq!(g.status, GoalStatus::Active);
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn account_exactly_at_budget_flips() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        let out = db.account_goal_usage("s", 1000, 5, None).await.unwrap();
        match out {
            GoalAccountingOutcome::Updated(g) => {
                assert_eq!(g.tokens_used, 1000);
                assert_eq!(g.status, GoalStatus::BudgetLimited);
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn account_over_budget_flips_and_records_full_delta() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        let out = db.account_goal_usage("s", 1500, 5, None).await.unwrap();
        match out {
            GoalAccountingOutcome::Updated(g) => {
                assert_eq!(g.tokens_used, 1500);
                assert_eq!(g.status, GoalStatus::BudgetLimited);
            }
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn account_no_budget_never_flips() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        let out = db
            .account_goal_usage("s", 10_000_000, 1, None)
            .await
            .unwrap();
        match out {
            GoalAccountingOutcome::Updated(g) => assert_eq!(g.status, GoalStatus::Active),
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn account_paused_goal_is_unchanged() {
        let db = db_with_session("s").await;
        let g = db
            .insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap()
            .unwrap();
        db.update_goal(
            "s",
            GoalUpdate {
                status: Some(GoalStatus::Paused),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let out = db.account_goal_usage("s", 500, 1, None).await.unwrap();
        match out {
            GoalAccountingOutcome::Unchanged(Some(g2)) => {
                assert_eq!(g2.tokens_used, 0);
                assert_eq!(g2.status, GoalStatus::Paused);
                assert_eq!(g2.goal_id, g.goal_id);
            }
            other => panic!("expected Unchanged(Some), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn account_with_wrong_expected_goal_id_is_unchanged() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        let out = db
            .account_goal_usage("s", 500, 1, Some("not-the-real-goal-id"))
            .await
            .unwrap();
        match out {
            GoalAccountingOutcome::Unchanged(Some(g)) => {
                assert_eq!(g.tokens_used, 0);
            }
            other => panic!("expected Unchanged(Some), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_goal_returns_none_when_missing() {
        let db = db_with_session("s").await;
        let out = db
            .update_goal(
                "s",
                GoalUpdate {
                    status: Some(GoalStatus::Paused),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn update_goal_objective_regenerates_id_and_resets_usage() {
        let db = db_with_session("s").await;
        let g1 = db
            .insert_goal("s", "first", GoalStatus::Active, None)
            .await
            .unwrap()
            .unwrap();
        db.account_goal_usage("s", 50, 1, None).await.unwrap();
        let g2 = db
            .update_goal(
                "s",
                GoalUpdate {
                    objective: Some("second".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(g2.goal_id, g1.goal_id);
        assert_eq!(g2.objective, "second");
        assert_eq!(g2.tokens_used, 0);
    }

    #[tokio::test]
    async fn update_goal_cas_rejects_when_id_mismatch() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        let out = db
            .update_goal(
                "s",
                GoalUpdate {
                    status: Some(GoalStatus::Paused),
                    expected_goal_id: Some("stale".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(out.is_none());
        assert_eq!(
            db.get_goal("s").await.unwrap().unwrap().status,
            GoalStatus::Active
        );
    }

    #[tokio::test]
    async fn update_goal_clears_budget_when_token_budget_is_some_none() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, Some(1000))
            .await
            .unwrap();
        let out = db
            .update_goal(
                "s",
                GoalUpdate {
                    token_budget: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.token_budget, None);
    }

    #[tokio::test]
    async fn delete_goal_returns_true_when_present_false_when_absent() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        assert!(db.delete_goal("s").await.unwrap());
        assert!(!db.delete_goal("s").await.unwrap());
    }

    #[tokio::test]
    async fn pause_active_goal_only_affects_active() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        let paused = db.pause_active_goal("s").await.unwrap().unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
        // Calling again on a Paused goal must be a no-op.
        assert!(db.pause_active_goal("s").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_session_cascades_goal() {
        let db = db_with_session("s").await;
        db.insert_goal("s", "x", GoalStatus::Active, None)
            .await
            .unwrap();
        db.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params!("s"))
            .await
            .unwrap();
        assert!(db.get_goal("s").await.unwrap().is_none());
    }
}
