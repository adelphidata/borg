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
