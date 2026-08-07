//! Persistence for session heartbeats: one recurring prompt per session that
//! re-enters that same conversation when it is idle.
//!
//! Deliberately *not* the same thing as [`crate::db::tasks`] / the heartbeat
//! scheduler. A scheduled task runs a fresh, isolated session per tick; a
//! session heartbeat reuses the live conversation, with all of its context. If
//! the recurring prompt needs what was said earlier in this chat, it belongs
//! here; if it is a self-contained job, it belongs in the scheduler.

use anyhow::Result;
use libsql::params;

use super::Database;

/// Floor on the firing interval.
///
/// Matches `heartbeat.min_interval_secs`: a prompt that re-enters a session
/// faster than once a minute is a runaway loop, not a check-in.
pub const MIN_HEARTBEAT_INTERVAL_SECS: i64 = 60;

/// A session's recurring prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeartbeat {
    /// Session that owns it.
    pub session_id: String,
    /// Prompt text injected as an ordinary user message.
    pub prompt: String,
    /// Seconds between fires; never below [`MIN_HEARTBEAT_INTERVAL_SECS`].
    pub interval_secs: i64,
    /// When true the heartbeat is retained but never fires.
    pub paused: bool,
    /// Unix seconds of the next eligible fire.
    pub next_fire_at: i64,
    /// Unix seconds.
    pub created_at: i64,
}

impl SessionHeartbeat {
    /// Seconds until the next fire, saturating at 0 when already due.
    pub fn seconds_until_due(&self, now: i64) -> i64 {
        (self.next_fire_at - now).max(0)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const HB_COLS: &str = "session_id, prompt, interval_secs, paused, next_fire_at, created_at";

fn map_row(row: &libsql::Row) -> Result<SessionHeartbeat> {
    Ok(SessionHeartbeat {
        session_id: row.get::<String>(0)?,
        prompt: row.get::<String>(1)?,
        interval_secs: row.get::<i64>(2)?,
        paused: row.get::<i64>(3)? != 0,
        next_fire_at: row.get::<i64>(4)?,
        created_at: row.get::<i64>(5)?,
    })
}

impl Database {
    /// The session's heartbeat, if one is set.
    pub async fn get_session_heartbeat(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionHeartbeat>> {
        let sql = format!("SELECT {HB_COLS} FROM session_heartbeats WHERE session_id = ?1");
        let mut rows = self.conn.query(&sql, params!(session_id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Set (or replace) the session's heartbeat.
    ///
    /// The interval is clamped up to [`MIN_HEARTBEAT_INTERVAL_SECS`] rather
    /// than rejected, and the first fire is anchored a full interval out — a
    /// heartbeat that fired the instant it was created would answer a question
    /// the user just asked in person.
    pub async fn set_session_heartbeat(
        &self,
        session_id: &str,
        prompt: &str,
        interval_secs: i64,
    ) -> Result<SessionHeartbeat> {
        let interval = interval_secs.max(MIN_HEARTBEAT_INTERVAL_SECS);
        let now = now_unix();
        let hb = SessionHeartbeat {
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
            interval_secs: interval,
            paused: false,
            next_fire_at: now + interval,
            created_at: now,
        };
        self.conn
            .execute(
                "INSERT INTO session_heartbeats
                    (session_id, prompt, interval_secs, paused, next_fire_at, created_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                    prompt = excluded.prompt,
                    interval_secs = excluded.interval_secs,
                    paused = 0,
                    next_fire_at = excluded.next_fire_at",
                params!(session_id, prompt, interval, hb.next_fire_at, hb.created_at),
            )
            .await?;
        Ok(hb)
    }

    /// Pause or resume. Returns the updated row, or `None` if none is set.
    ///
    /// Resuming re-anchors the timer: a heartbeat paused for a week must not
    /// fire the instant it comes back.
    pub async fn set_session_heartbeat_paused(
        &self,
        session_id: &str,
        paused: bool,
    ) -> Result<Option<SessionHeartbeat>> {
        let now = now_unix();
        self.conn
            .execute(
                "UPDATE session_heartbeats
                    SET paused = ?2,
                        next_fire_at = CASE WHEN ?2 = 0
                                            THEN ?3 + interval_secs
                                            ELSE next_fire_at END
                  WHERE session_id = ?1",
                params!(session_id, i64::from(paused), now),
            )
            .await?;
        self.get_session_heartbeat(session_id).await
    }

    /// Delete the heartbeat. Returns whether a row was removed.
    pub async fn clear_session_heartbeat(&self, session_id: &str) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM session_heartbeats WHERE session_id = ?1",
                params!(session_id),
            )
            .await?;
        Ok(affected > 0)
    }

    /// Every unpaused heartbeat whose `next_fire_at` has passed.
    ///
    /// The poller intersects this with the set of *live, idle* sessions, so a
    /// row for a session that is not currently hosted simply waits.
    pub async fn due_session_heartbeats(&self, now: i64) -> Result<Vec<SessionHeartbeat>> {
        let sql = format!(
            "SELECT {HB_COLS} FROM session_heartbeats
              WHERE paused = 0 AND next_fire_at <= ?1
              ORDER BY next_fire_at"
        );
        let mut rows = self.conn.query(&sql, params!(now)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_row(&row)?);
        }
        Ok(out)
    }

    /// Re-anchor the timer after a fire.
    ///
    /// Anchors on *now*, not on the previous `next_fire_at`. That is what makes
    /// missed ticks coalesce: a session busy (or a daemon down) through six
    /// intervals fires once and then waits a full interval, instead of firing
    /// six times in a row to "catch up" on check-ins nobody wanted.
    pub async fn reanchor_session_heartbeat(&self, session_id: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE session_heartbeats
                    SET next_fire_at = ?2 + interval_secs
                  WHERE session_id = ?1",
                params!(session_id, now),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db_with_session(id: &str) -> Database {
        let db = Database::test_db().await;
        db.upsert_session(id, 0, 0, 0, "m", "test").await.unwrap();
        db
    }

    #[tokio::test]
    async fn interval_is_clamped_to_the_floor_rather_than_rejected() {
        // A sub-minute heartbeat is a runaway loop. Clamping keeps `/heartbeat
        // every 5s ...` useful instead of erroring the user out.
        let db = db_with_session("s").await;
        let hb = db.set_session_heartbeat("s", "check ci", 5).await.unwrap();
        assert_eq!(hb.interval_secs, MIN_HEARTBEAT_INTERVAL_SECS);

        let hb = db
            .set_session_heartbeat("s", "check ci", 900)
            .await
            .unwrap();
        assert_eq!(hb.interval_secs, 900, "a sane interval is kept verbatim");
    }

    #[tokio::test]
    async fn a_new_heartbeat_does_not_fire_immediately() {
        // Anchoring at creation time would fire a check-in in the same breath
        // as the user setting it up.
        let db = db_with_session("s").await;
        let hb = db.set_session_heartbeat("s", "p", 300).await.unwrap();
        assert!(hb.next_fire_at >= hb.created_at + 300);
        assert!(db
            .due_session_heartbeats(hb.created_at)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn setting_a_heartbeat_twice_replaces_it_and_unpauses() {
        let db = db_with_session("s").await;
        db.set_session_heartbeat("s", "first", 300).await.unwrap();
        db.set_session_heartbeat_paused("s", true).await.unwrap();

        let hb = db.set_session_heartbeat("s", "second", 600).await.unwrap();
        assert_eq!(hb.prompt, "second");
        assert_eq!(hb.interval_secs, 600);
        assert!(!hb.paused, "re-setting must resume a paused heartbeat");
    }

    #[tokio::test]
    async fn paused_heartbeats_are_never_due_and_resume_re_anchors() {
        let db = db_with_session("s").await;
        let hb = db.set_session_heartbeat("s", "p", 60).await.unwrap();
        let long_after = hb.next_fire_at + 10_000;

        assert_eq!(
            db.due_session_heartbeats(long_after).await.unwrap().len(),
            1
        );

        db.set_session_heartbeat_paused("s", true).await.unwrap();
        assert!(
            db.due_session_heartbeats(long_after)
                .await
                .unwrap()
                .is_empty(),
            "a paused heartbeat must never fire"
        );

        // Resuming must not immediately fire the tick it slept through: the
        // anchor is resume time, not the stale deadline from before the pause.
        let at_resume = now_unix();
        let resumed = db
            .set_session_heartbeat_paused("s", false)
            .await
            .unwrap()
            .unwrap();
        assert!(!resumed.paused);
        assert!(
            resumed.next_fire_at >= at_resume + 60,
            "resume must re-anchor a full interval out, got {} vs resume time {at_resume}",
            resumed.next_fire_at
        );
        assert!(
            db.due_session_heartbeats(at_resume)
                .await
                .unwrap()
                .is_empty(),
            "a just-resumed heartbeat must not be due"
        );
    }

    #[tokio::test]
    async fn missed_ticks_coalesce_into_one_fire() {
        // The behavior that separates this from a job queue: a session busy
        // through several intervals owes exactly one check-in, not a backlog.
        let db = db_with_session("s").await;
        let hb = db.set_session_heartbeat("s", "p", 60).await.unwrap();
        let six_intervals_later = hb.next_fire_at + 6 * 60;

        assert_eq!(
            db.due_session_heartbeats(six_intervals_later)
                .await
                .unwrap()
                .len(),
            1,
            "a long silence still produces a single due row"
        );

        db.reanchor_session_heartbeat("s", six_intervals_later)
            .await
            .unwrap();
        assert!(
            db.due_session_heartbeats(six_intervals_later)
                .await
                .unwrap()
                .is_empty(),
            "after firing, the next fire is a full interval out — no catch-up burst"
        );
        let after = db.get_session_heartbeat("s").await.unwrap().unwrap();
        assert_eq!(after.next_fire_at, six_intervals_later + 60);
    }

    #[tokio::test]
    async fn clear_removes_it_and_is_a_no_op_the_second_time() {
        let db = db_with_session("s").await;
        db.set_session_heartbeat("s", "p", 60).await.unwrap();
        assert!(db.clear_session_heartbeat("s").await.unwrap());
        assert!(!db.clear_session_heartbeat("s").await.unwrap());
        assert!(db.get_session_heartbeat("s").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_the_session_cascades_the_heartbeat() {
        let db = db_with_session("s").await;
        db.set_session_heartbeat("s", "p", 60).await.unwrap();
        db.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params!("s"))
            .await
            .unwrap();
        assert!(db.get_session_heartbeat("s").await.unwrap().is_none());
    }
}
