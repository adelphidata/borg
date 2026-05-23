//! `/handoff` cross-platform session transfer — DB state machine.
//!
//! A handoff moves an in-flight session from the originating surface (CLI,
//! TUI) to a destination gateway channel (Telegram, Slack, …). The four
//! columns on the `sessions` row drive the state machine:
//!
//! ```text
//!     None  --request-->  pending  --claim-->  running  --complete-->  completed
//!                            |                    |
//!                            +-- fail --+         +-- fail --> failed
//! ```
//!
//! - `request` is the only path into `pending` — initiated by the operator
//!   via `/handoff <platform>` (CLI) or `RequestHandoff` (gRPC).
//! - `claim` is what the destination gateway calls when it picks up the
//!   pending row to begin serving the session.
//! - `complete` / `fail` finalize the state. A failed claim records the
//!   reason in `handoff_error` so the originator can surface it.

use anyhow::{bail, Result};
use libsql::params;

use super::Database;

/// Lifecycle states the handoff column can take. Encoded as TEXT in SQLite so
/// the column is easy to inspect via the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffState {
    /// Operator requested a handoff; the destination gateway hasn't picked it
    /// up yet.
    Pending,
    /// Destination gateway has claimed the row and is serving the session.
    Running,
    /// Handoff completed successfully — the session is now owned by the
    /// destination surface.
    Completed,
    /// Handoff failed; see `handoff_error` for the reason.
    Failed,
}

impl HandoffState {
    /// Stable on-disk encoding. Never rename — operators query this column
    /// directly via `sqlite3 ~/.borg/borg.db`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// Inverse of `as_str`. Returns `None` for unknown strings so a DB-level
    /// typo never silently desynchronizes the state machine.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Snapshot of one row's handoff columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffStatus {
    /// Current state — `None` when no handoff has ever been requested.
    pub state: Option<HandoffState>,
    /// Destination channel name (`"telegram"`, `"slack"`, …) when state is set.
    pub target: Option<String>,
    /// Failure reason when state == Failed.
    pub error: Option<String>,
    /// Unix seconds of the initial request, when state is set.
    pub requested_at: Option<i64>,
}

impl Database {
    /// Mark `session_id` as pending handoff to `target_platform`. Refuses if
    /// the row is already in a non-terminal handoff state — the operator
    /// must `fail` or `complete` the existing handoff first.
    pub async fn request_handoff(&self, session_id: &str, target_platform: &str) -> Result<()> {
        if target_platform.trim().is_empty() {
            bail!("handoff target platform is empty");
        }
        let now = chrono::Utc::now().timestamp();
        let current = self.handoff_status(session_id).await?;
        if let Some(state @ (HandoffState::Pending | HandoffState::Running)) = current.state {
            bail!(
                "session {session_id} already has a {} handoff to {}",
                state.as_str(),
                current.target.unwrap_or_default(),
            );
        }
        let changes = self
            .conn
            .execute(
                "UPDATE sessions
                    SET handoff_state = 'pending',
                        handoff_target = ?2,
                        handoff_error = NULL,
                        handoff_requested_at = ?3
                    WHERE id = ?1",
                params!(session_id, target_platform, now),
            )
            .await?;
        if changes == 0 {
            bail!("session {session_id} not found");
        }
        Ok(())
    }

    /// Atomically flip `pending → running` for any row targeting `platform`.
    /// Returns the claimed session id, or `None` if there's nothing to claim.
    /// Called by the destination gateway's poll loop.
    ///
    /// Two-step SELECT-then-UPDATE-where-state, retried a bounded number of
    /// times so that under contention (two claimants polling the same
    /// platform with multiple pending rows queued) the loser still picks up
    /// the next eligible row instead of idling a tick. The
    /// `WHERE handoff_state = 'pending'` clause on the UPDATE serializes
    /// claimants on the same row — loser sees `changes == 0` and retries.
    pub async fn claim_handoff_for_platform(&self, platform: &str) -> Result<Option<String>> {
        const MAX_ATTEMPTS: usize = 3;
        for _ in 0..MAX_ATTEMPTS {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM sessions
                        WHERE handoff_state = 'pending' AND handoff_target = ?1
                        ORDER BY handoff_requested_at ASC, id ASC LIMIT 1",
                    params!(platform),
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let id: String = row.get(0)?;
            let changes = self
                .conn
                .execute(
                    "UPDATE sessions
                        SET handoff_state = 'running'
                        WHERE id = ?1 AND handoff_state = 'pending'",
                    params!(id.clone()),
                )
                .await?;
            if changes == 1 {
                return Ok(Some(id));
            }
            // Lost the race for this id — another claimant beat us. Loop to
            // try the next pending row.
        }
        Ok(None)
    }

    /// Mark a running handoff as completed. Errors if the row isn't running.
    pub async fn complete_handoff(&self, session_id: &str) -> Result<()> {
        let changes = self
            .conn
            .execute(
                "UPDATE sessions
                    SET handoff_state = 'completed'
                    WHERE id = ?1 AND handoff_state = 'running'",
                params!(session_id),
            )
            .await?;
        if changes == 0 {
            bail!("session {session_id} is not in `running` handoff state");
        }
        Ok(())
    }

    /// Mark a handoff as failed with a human-readable reason. Allowed from
    /// any non-terminal state (pending or running) so callers can fail-fast
    /// regardless of whether the destination claimed the row.
    pub async fn fail_handoff(&self, session_id: &str, reason: &str) -> Result<()> {
        let changes = self
            .conn
            .execute(
                "UPDATE sessions
                    SET handoff_state = 'failed', handoff_error = ?2
                    WHERE id = ?1 AND handoff_state IN ('pending', 'running')",
                params!(session_id, reason),
            )
            .await?;
        if changes == 0 {
            bail!("session {session_id} is not in a failable handoff state");
        }
        Ok(())
    }

    /// Read the current handoff status for a single session.
    pub async fn handoff_status(&self, session_id: &str) -> Result<HandoffStatus> {
        let mut rows = self
            .conn
            .query(
                "SELECT handoff_state, handoff_target, handoff_error, handoff_requested_at
                    FROM sessions
                    WHERE id = ?1",
                params!(session_id),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(HandoffStatus {
                state: None,
                target: None,
                error: None,
                requested_at: None,
            });
        };
        let state_str: Option<String> = row.get(0)?;
        Ok(HandoffStatus {
            state: state_str.as_deref().and_then(HandoffState::from_str),
            target: row.get::<Option<String>>(1)?,
            error: row.get::<Option<String>>(2)?,
            requested_at: row.get::<Option<i64>>(3)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_session(db: &Database, id: &str) {
        let now = chrono::Utc::now().timestamp();
        db.conn
            .execute(
                "INSERT INTO sessions (id, created_at, updated_at) VALUES (?1, ?2, ?2)",
                params!(id, now),
            )
            .await
            .expect("seed session");
    }

    #[tokio::test]
    async fn happy_path_request_claim_complete() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        let status = db.handoff_status("s1").await.unwrap();
        assert_eq!(status.state, Some(HandoffState::Pending));
        assert_eq!(status.target.as_deref(), Some("telegram"));

        let claimed = db.claim_handoff_for_platform("telegram").await.unwrap();
        assert_eq!(claimed.as_deref(), Some("s1"));
        assert_eq!(
            db.handoff_status("s1").await.unwrap().state,
            Some(HandoffState::Running)
        );

        db.complete_handoff("s1").await.unwrap();
        assert_eq!(
            db.handoff_status("s1").await.unwrap().state,
            Some(HandoffState::Completed)
        );
    }

    #[tokio::test]
    async fn second_request_while_pending_is_refused() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        // A second request must refuse — otherwise an operator typo silently
        // retargets a pending handoff and the wrong gateway claims it.
        let err = db.request_handoff("s1", "slack").await.unwrap_err();
        assert!(err.to_string().contains("pending"));
    }

    #[tokio::test]
    async fn claim_only_matches_target_platform() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        // Slack gateway must not steal a Telegram-targeted handoff.
        assert!(db
            .claim_handoff_for_platform("slack")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            db.handoff_status("s1").await.unwrap().state,
            Some(HandoffState::Pending)
        );
    }

    #[tokio::test]
    async fn concurrent_claim_only_one_wins() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        // Simulated race: two claim attempts back-to-back. Only the first
        // should observe the row in `pending`; the second sees it `running`
        // and gets `None`.
        let first = db.claim_handoff_for_platform("telegram").await.unwrap();
        let second = db.claim_handoff_for_platform("telegram").await.unwrap();
        assert!(first.is_some());
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn fail_records_reason() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        db.fail_handoff("s1", "destination unreachable")
            .await
            .unwrap();
        let status = db.handoff_status("s1").await.unwrap();
        assert_eq!(status.state, Some(HandoffState::Failed));
        assert_eq!(status.error.as_deref(), Some("destination unreachable"));
    }

    #[tokio::test]
    async fn fail_from_terminal_state_is_refused() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        db.claim_handoff_for_platform("telegram").await.unwrap();
        db.complete_handoff("s1").await.unwrap();
        // Already terminal — a stray fail after completion would mask a real
        // success. Must refuse.
        assert!(db.fail_handoff("s1", "too late").await.is_err());
    }

    #[tokio::test]
    async fn completed_handoff_allows_a_new_one() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        db.request_handoff("s1", "telegram").await.unwrap();
        db.claim_handoff_for_platform("telegram").await.unwrap();
        db.complete_handoff("s1").await.unwrap();
        // Operator can hand off again — common pattern is desktop → phone →
        // desktop over the lifetime of a long session.
        db.request_handoff("s1", "slack").await.unwrap();
        assert_eq!(
            db.handoff_status("s1").await.unwrap().target.as_deref(),
            Some("slack")
        );
    }

    #[tokio::test]
    async fn request_with_empty_target_refuses() {
        let db = Database::test_db().await;
        seed_session(&db, "s1").await;
        let err = db.request_handoff("s1", "   ").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn handoff_status_for_unknown_session_returns_empty_status() {
        let db = Database::test_db().await;
        let status = db.handoff_status("never-existed").await.unwrap();
        assert_eq!(status.state, None);
    }
}
