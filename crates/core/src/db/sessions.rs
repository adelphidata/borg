use anyhow::Result;
use rusqlite::params;
use tracing::instrument;

use super::models::{MessageRow, SessionRow};
use super::Database;

impl Database {
    pub fn upsert_session(
        &self,
        id: &str,
        created_at: i64,
        updated_at: i64,
        total_tokens: i64,
        model: &str,
        title: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, created_at, updated_at, total_tokens, model, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                updated_at = ?3, total_tokens = ?4, model = ?5, title = ?6",
            params![id, created_at, updated_at, total_tokens, model, title],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, updated_at, total_tokens, model, title
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    total_tokens: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Return sessions updated since a given Unix timestamp, ordered by most recent first.
    pub fn sessions_since(&self, since: i64) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, updated_at, total_tokens, model, title
             FROM sessions WHERE updated_at >= ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map(params![since], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    total_tokens: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── Message persistence ──

    #[instrument(skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub fn insert_message(
        &self,
        session_id: &str,
        role: &str,
        content: Option<&str>,
        tool_calls_json: Option<&str>,
        tool_call_id: Option<&str>,
        timestamp: Option<&str>,
        content_parts_json: Option<&str>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls_json, tool_call_id, timestamp, created_at, content_parts_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![session_id, role, content, tool_calls_json, tool_call_id, timestamp, now, content_parts_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn load_session_messages(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, tool_calls_json, tool_call_id, timestamp, created_at, content_parts_json
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(MessageRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    tool_calls_json: row.get(4)?,
                    tool_call_id: row.get(5)?,
                    timestamp: row.get(6)?,
                    created_at: row.get(7)?,
                    content_parts_json: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_session_messages(&self, session_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(count)
    }

    // ── Channel sessions ──

    pub fn resolve_channel_session(&self, channel_name: &str, sender_id: &str) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM channel_sessions WHERE channel_name = ?1 AND sender_id = ?2",
        )?;
        let existing: Option<String> = stmt
            .query_map(params![channel_name, sender_id], |row| row.get(0))?
            .next()
            .and_then(std::result::Result::ok);

        match existing {
            Some(session_id) => {
                self.conn.execute(
                    "UPDATE channel_sessions SET last_active = ?1 WHERE channel_name = ?2 AND sender_id = ?3",
                    params![now, channel_name, sender_id],
                )?;
                Ok(session_id)
            }
            None => {
                let session_id = uuid::Uuid::new_v4().to_string();
                self.conn.execute(
                    "INSERT INTO channel_sessions (channel_name, sender_id, session_id, created_at, last_active)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![channel_name, sender_id, session_id, now, now],
                )?;
                Ok(session_id)
            }
        }
    }

    pub fn log_channel_message(
        &self,
        channel_name: &str,
        sender_id: &str,
        direction: &str,
        content: Option<&str>,
        metadata_json: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO channel_messages (channel_name, sender_id, direction, content, metadata_json, session_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![channel_name, sender_id, direction, content, metadata_json, session_id, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update session_id for a channel session (used by /new command).
    pub fn update_channel_session_id(
        &self,
        channel_name: &str,
        sender_id: &str,
        new_session_id: &str,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let updated = self.conn.execute(
            "UPDATE channel_sessions SET session_id = ?1, last_active = ?2 WHERE channel_name = ?3 AND sender_id = ?4",
            params![new_session_id, now, channel_name, sender_id],
        )?;
        Ok(updated > 0)
    }

    /// Count messages in a session.
    pub fn count_session_messages(&self, session_id: &str) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM messages WHERE session_id = ?1")?;
        let count: i64 = stmt.query_row(params![session_id], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Keep the last `keep` messages in a session, deleting older ones.
    /// Returns the number of deleted messages.
    pub fn compact_session_messages(&self, session_id: &str, keep: usize) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND id NOT IN \
             (SELECT id FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![session_id, keep as i64],
        )?;
        Ok(count)
    }

    /// Delete the last assistant turn (assistant + tool messages) from a session.
    /// Walks backwards from the end, collecting messages until a `user` message is
    /// hit. Returns the number of deleted messages.
    pub fn delete_last_assistant_turn(&self, session_id: &str) -> Result<usize> {
        let messages = self.load_session_messages(session_id)?;
        let mut ids_to_delete = Vec::new();
        for msg in messages.iter().rev() {
            if msg.role == "user" {
                break;
            }
            ids_to_delete.push(msg.id);
        }
        if ids_to_delete.is_empty() {
            return Ok(0);
        }
        let placeholders: Vec<String> = ids_to_delete.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "DELETE FROM messages WHERE session_id = ?1 AND id IN ({})",
            placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut param_idx = 1;
        stmt.raw_bind_parameter(param_idx, session_id)?;
        for id in &ids_to_delete {
            param_idx += 1;
            stmt.raw_bind_parameter(param_idx, id)?;
        }
        let count = stmt.raw_execute()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::test_db()
    }

    #[test]
    fn test_upsert_and_list_sessions() {
        let db = test_db();
        db.upsert_session("s1", 100, 200, 500, "gpt-4", "Session One")
            .unwrap();
        db.upsert_session("s2", 150, 300, 1000, "claude", "Session Two")
            .unwrap();

        let sessions = db.list_sessions(10).unwrap();
        assert!(sessions.len() >= 2);
        // Most recent updated_at first
        let s2_idx = sessions.iter().position(|s| s.id == "s2").unwrap();
        let s1_idx = sessions.iter().position(|s| s.id == "s1").unwrap();
        assert!(
            s2_idx < s1_idx,
            "s2 (updated_at=300) should come before s1 (updated_at=200)"
        );
    }

    #[test]
    fn test_upsert_session_updates_existing() {
        let db = test_db();
        db.upsert_session("s1", 100, 200, 500, "gpt-4", "Original")
            .unwrap();
        db.upsert_session("s1", 100, 300, 1000, "claude", "Updated")
            .unwrap();

        let sessions = db.list_sessions(100).unwrap();
        let s1 = sessions.iter().find(|s| s.id == "s1").unwrap();
        assert_eq!(s1.title, "Updated");
        assert_eq!(s1.total_tokens, 1000);
        assert_eq!(s1.model, "claude");
        // Should not have duplicates
        assert_eq!(sessions.iter().filter(|s| s.id == "s1").count(), 1);
    }

    #[test]
    fn test_list_sessions_respects_limit() {
        let db = test_db();
        for i in 0..5 {
            db.upsert_session(
                &format!("s{i}"),
                100 + i,
                200 + i,
                0,
                "model",
                &format!("Session {i}"),
            )
            .unwrap();
        }
        let sessions = db.list_sessions(2).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_sessions_since() {
        let db = test_db();
        db.upsert_session("s1", 50, 100, 0, "m", "old").unwrap();
        db.upsert_session("s2", 50, 200, 0, "m", "mid").unwrap();
        db.upsert_session("s3", 50, 300, 0, "m", "new").unwrap();

        let recent = db.sessions_since(150).unwrap();
        let ids: Vec<&str> = recent.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"s2"));
        assert!(ids.contains(&"s3"));
        assert!(!ids.contains(&"s1"));
    }

    #[test]
    fn test_insert_and_load_messages() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();

        db.insert_message("s1", "user", Some("hello"), None, None, None, None)
            .unwrap();
        db.insert_message(
            "s1",
            "assistant",
            Some("hi there"),
            Some(r#"[{"id":"tc1"}]"#),
            None,
            Some("2024-01-01T00:00:00Z"),
            None,
        )
        .unwrap();
        db.insert_message("s1", "tool", Some("result"), None, Some("tc1"), None, None)
            .unwrap();

        let msgs = db.load_session_messages("s1").unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content.as_deref(), Some("hello"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(
            msgs[1].tool_calls_json.as_deref(),
            Some(r#"[{"id":"tc1"}]"#)
        );
        assert_eq!(msgs[1].timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(msgs[2].role, "tool");
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc1"));
        // Verify ASC order by id
        assert!(msgs[0].id < msgs[1].id);
        assert!(msgs[1].id < msgs[2].id);
    }

    #[test]
    fn test_delete_session_messages() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();
        db.insert_message("s1", "user", Some("a"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "assistant", Some("b"), None, None, None, None)
            .unwrap();

        let deleted = db.delete_session_messages("s1").unwrap();
        assert_eq!(deleted, 2);

        let msgs = db.load_session_messages("s1").unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_resolve_channel_session_creates_new() {
        let db = test_db();
        let sid1 = db.resolve_channel_session("telegram", "user1").unwrap();
        let sid2 = db.resolve_channel_session("telegram", "user1").unwrap();
        assert_eq!(sid1, sid2, "same channel+sender should return same session");
    }

    #[test]
    fn test_resolve_channel_session_different_senders() {
        let db = test_db();
        let sid1 = db.resolve_channel_session("telegram", "user1").unwrap();
        let sid2 = db.resolve_channel_session("telegram", "user2").unwrap();
        assert_ne!(
            sid1, sid2,
            "different senders should get different sessions"
        );
    }

    #[test]
    fn test_compact_session_messages() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();
        for i in 0..10 {
            db.insert_message(
                "s1",
                "user",
                Some(&format!("msg{i}")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }

        let deleted = db.compact_session_messages("s1", 3).unwrap();
        assert_eq!(deleted, 7);

        let remaining = db.load_session_messages("s1").unwrap();
        assert_eq!(remaining.len(), 3);
        // The newest 3 should remain
        assert_eq!(remaining[0].content.as_deref(), Some("msg7"));
        assert_eq!(remaining[1].content.as_deref(), Some("msg8"));
        assert_eq!(remaining[2].content.as_deref(), Some("msg9"));
    }

    #[test]
    fn test_delete_last_assistant_turn() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();
        // user -> assistant -> tool -> user -> assistant -> tool
        db.insert_message("s1", "user", Some("q1"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "assistant", Some("a1"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "tool", Some("t1"), None, Some("tc1"), None, None)
            .unwrap();
        db.insert_message("s1", "user", Some("q2"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "assistant", Some("a2"), None, None, None, None)
            .unwrap();
        db.insert_message("s1", "tool", Some("t2"), None, Some("tc2"), None, None)
            .unwrap();

        let deleted = db.delete_last_assistant_turn("s1").unwrap();
        assert_eq!(deleted, 2); // assistant + tool

        let msgs = db.load_session_messages("s1").unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3].role, "user");
        assert_eq!(msgs[3].content.as_deref(), Some("q2"));
    }

    #[test]
    fn test_delete_last_assistant_turn_only_user_messages() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();
        db.insert_message("s1", "user", Some("q1"), None, None, None, None)
            .unwrap();

        let deleted = db.delete_last_assistant_turn("s1").unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_log_channel_message() {
        let db = test_db();
        let rowid = db
            .log_channel_message("telegram", "user1", "inbound", Some("hello"), None, None)
            .unwrap();
        assert!(rowid > 0);
    }

    #[test]
    fn test_update_channel_session_id() {
        let db = test_db();
        let _sid = db.resolve_channel_session("telegram", "user1").unwrap();

        let updated = db
            .update_channel_session_id("telegram", "user1", "new-session-id")
            .unwrap();
        assert!(updated);

        // Non-existent returns false
        let not_found = db
            .update_channel_session_id("telegram", "nobody", "x")
            .unwrap();
        assert!(!not_found);
    }

    #[test]
    fn test_count_session_messages() {
        let db = test_db();
        db.upsert_session("s1", 100, 100, 0, "m", "test").unwrap();
        for _ in 0..5 {
            db.insert_message("s1", "user", Some("msg"), None, None, None, None)
                .unwrap();
        }
        assert_eq!(db.count_session_messages("s1").unwrap(), 5);
    }
}
