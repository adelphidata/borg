use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use tracing::{instrument, warn};

use super::models::{ChannelActivityRow, MessageRow, SessionRow};
use super::Database;

/// Hard cap on rows returned by [`Database::sessions_since`]. Acts as a
/// circuit-breaker against unbounded scans (e.g. heartbeat passes `since=0`).
/// Sized well above any realistic working-set so legitimate queries are not
/// silently truncated; if a deployment hits this we have a separate problem.
const SESSIONS_SINCE_HARD_CAP: i64 = 10_000;

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

    /// Fetch a single session row by exact id. Returns `None` if absent.
    pub fn session_by_id(&self, id: &str) -> Result<Option<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
             FROM sessions WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    total_tokens: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                    parent_session_id: row.get(6)?,
                    forked_from_message_id: row.get(7)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
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
                    parent_session_id: row.get(6)?,
                    forked_from_message_id: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Return sessions updated since a given Unix timestamp, ordered by most recent first.
    ///
    /// Capped at [`SESSIONS_SINCE_HARD_CAP`] rows as a circuit-breaker. Callers
    /// like the heartbeat invoke this with `since=0` (= "all sessions"), which
    /// would otherwise scan the entire table for power users.
    pub fn sessions_since(&self, since: i64) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
             FROM sessions WHERE updated_at >= ?1 ORDER BY updated_at DESC
             LIMIT ?2",
        )?;
        let rows: Vec<SessionRow> = stmt
            .query_map(params![since, SESSIONS_SINCE_HARD_CAP], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    total_tokens: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                    parent_session_id: row.get(6)?,
                    forked_from_message_id: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() == SESSIONS_SINCE_HARD_CAP as usize {
            // Hitting the cap means a future caller that walks the full set
            // will silently process a stale tail. Surface it so we notice
            // before someone relies on completeness.
            warn!(
                "sessions_since hit the {SESSIONS_SINCE_HARD_CAP}-row cap (since={since}); \
                 results are truncated"
            );
        }
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

    /// Create a forked session by copying message rows from `parent_id`
    /// up to and including `anchor_message_id` into a brand-new session row
    /// `new_id`. Done in a single transaction so partial copies never leak.
    ///
    /// Returns the number of messages copied. Errors if the anchor doesn't
    /// exist in the parent session, or if the parent session row is missing.
    pub fn fork_session(
        &self,
        parent_id: &str,
        anchor_message_id: i64,
        new_id: &str,
        new_title: &str,
        now: i64,
    ) -> Result<usize> {
        let parent = self
            .session_by_id(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("parent session '{parent_id}' not found"))?;
        let anchor_in_parent: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM messages WHERE session_id = ?1 AND id = ?2",
                params![parent_id, anchor_message_id],
                |row| row.get(0),
            )
            .optional()?;
        if anchor_in_parent.is_none() {
            anyhow::bail!("anchor message {anchor_message_id} not found in session '{parent_id}'");
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions
                (id, created_at, updated_at, total_tokens, model, title,
                 parent_session_id, forked_from_message_id)
             VALUES (?1, ?2, ?2, 0, ?3, ?4, ?5, ?6)",
            params![
                new_id,
                now,
                parent.model,
                new_title,
                parent_id,
                anchor_message_id
            ],
        )?;
        let copied = tx.execute(
            "INSERT INTO messages
                (session_id, role, content, tool_calls_json, tool_call_id,
                 timestamp, created_at, content_parts_json)
             SELECT ?1, role, content, tool_calls_json, tool_call_id,
                    timestamp, created_at, content_parts_json
             FROM messages
             WHERE session_id = ?2 AND id <= ?3
             ORDER BY id ASC",
            params![new_id, parent_id, anchor_message_id],
        )?;
        tx.commit()?;
        Ok(copied)
    }

    pub fn delete_session_messages(&self, session_id: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(count)
    }

    // ── Channel sessions ──

    /// Look up an existing channel session WITHOUT creating one. Returns
    /// `Ok(None)` when no row exists yet — used by read-only paths like
    /// `/btw` that need the current transcript snapshot but must not
    /// materialise a new session as a side effect.
    pub fn lookup_channel_session(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM channel_sessions WHERE channel_name = ?1 AND sender_id = ?2",
        )?;
        let existing: Option<String> = stmt
            .query_map(params![channel_name, sender_id], |row| row.get(0))?
            .next()
            .and_then(std::result::Result::ok);
        Ok(existing)
    }

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

    /// Count sessions whose `updated_at` is at or after `since_ts`. A session
    /// counts as "active in window" if it was touched, not merely created
    /// before the window — matches the wording in `/usage`.
    pub fn session_count_since(&self, since_ts: i64) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM sessions WHERE updated_at >= ?1")?;
        let count: i64 = stmt.query_row(params![since_ts], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    /// Count messages whose `created_at` is at or after `since_ts`.
    pub fn message_count_since(&self, since_ts: i64) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM messages WHERE created_at >= ?1")?;
        let count: i64 = stmt.query_row(params![since_ts], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    /// Aggregate tool-call counts across `messages.tool_calls_json` for rows
    /// at or after `since_ts`, returning up to `top_n` `(name, count)` pairs
    /// sorted by count descending.
    ///
    /// Two on-disk shapes are tolerated:
    /// - OpenAI/Codex style: `[{ "function": { "name": "..." }, ... }, ...]`
    /// - Anthropic-style assistant tool blocks: `[{ "name": "...", "input": {...} }, ...]`
    ///
    /// Rows whose `tool_calls_json` fails to parse are skipped with a
    /// `tracing::warn!` and do not abort the aggregation — `/usage` must
    /// not fail if one corrupted row exists somewhere in history.
    #[instrument(skip_all)]
    pub fn tool_call_counts_since(
        &self,
        since_ts: i64,
        top_n: usize,
    ) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool_calls_json FROM messages
             WHERE created_at >= ?1 AND tool_calls_json IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![since_ts], |row| row.get::<_, String>(0))?;

        let mut counts: HashMap<String, u64> = HashMap::new();
        for row in rows {
            let json = match row {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("tool_call_counts_since: row read failed: {e}");
                    continue;
                }
            };
            let parsed: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("tool_call_counts_since: skipping malformed row: {e}");
                    continue;
                }
            };
            let arr = match parsed.as_array() {
                Some(a) => a,
                None => {
                    tracing::warn!("tool_call_counts_since: tool_calls_json not an array");
                    continue;
                }
            };
            for call in arr {
                // OpenAI shape: { "function": { "name": "..." } }
                // Anthropic-inline: { "name": "..." }
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .or_else(|| call.get("name").and_then(|n| n.as_str()));
                if let Some(n) = name {
                    *counts.entry(n.to_string()).or_insert(0) += 1;
                }
            }
        }

        let mut sorted: Vec<(String, u64)> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        sorted.truncate(top_n);
        Ok(sorted)
    }

    /// Per-channel message activity for `channel_messages` rows at or after
    /// `since_ts`. `last_active_unix` is the MAX `created_at` for the channel
    /// in the window (asserting MAX, not arbitrary-row, is a tested
    /// invariant). Sorted by message count descending.
    #[instrument(skip_all)]
    pub fn channel_activity_since(&self, since_ts: i64) -> Result<Vec<ChannelActivityRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT channel_name, COUNT(*) AS msg_count, MAX(created_at) AS last_active
             FROM channel_messages
             WHERE created_at >= ?1
             GROUP BY channel_name
             ORDER BY msg_count DESC, channel_name ASC",
        )?;
        let rows = stmt
            .query_map(params![since_ts], |row| {
                let count: i64 = row.get(1)?;
                Ok(ChannelActivityRow {
                    channel_name: row.get(0)?,
                    message_count: count.max(0) as u64,
                    last_active_unix: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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

    /// FTS5 search over past session messages. Returns matches ordered by
    /// BM25 relevance. `MessageFtsHit` carries the session_id so the caller
    /// can fetch surrounding context if desired.
    ///
    /// Query is passed through a minimal sanitizer: strips FTS5 operators
    /// that could cause parse errors and quotes each word for phrase safety.
    pub fn messages_fts_search(&self, query: &str, limit: usize) -> Result<Vec<MessageFtsHit>> {
        let sanitized = sanitize_messages_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.session_id, m.role, m.content, m.created_at,
                    -bm25(messages_fts) AS score
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY score DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![sanitized, limit as i64], |row| {
                Ok(MessageFtsHit {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    created_at: row.get(4)?,
                    score: row.get::<_, f64>(5)? as f32,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// A single FTS hit from `messages_fts`.
#[derive(Debug, Clone)]
pub struct MessageFtsHit {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
    pub score: f32,
}

/// Strip FTS5 syntax characters that would cause a parse error, then quote
/// each word to match it as a literal phrase fragment. Kept narrower than the
/// `memory_chunks` sanitizer because session messages aren't chunked — we
/// only need to handle arbitrary user queries safely.
fn sanitize_messages_fts_query(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| match c {
            '"' | '(' | ')' | ':' | '*' | '^' => ' ',
            c => c,
        })
        .collect();
    let words: Vec<String> = cleaned
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect();
    words.join(" ")
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
    fn fork_copies_messages_up_to_anchor_inclusive() {
        let db = test_db();
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .unwrap();
        let mut ids = Vec::new();
        for i in 0..6 {
            ids.push(
                db.insert_message(
                    "parent",
                    "user",
                    Some(&format!("m{i}")),
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        }
        let anchor = ids[3];

        let copied = db
            .fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .unwrap();
        assert_eq!(copied, 4);

        let parent_msgs = db.load_session_messages("parent").unwrap();
        let child_msgs = db.load_session_messages("child").unwrap();
        assert_eq!(parent_msgs.len(), 6);
        assert_eq!(child_msgs.len(), 4);
        let parent_contents: Vec<_> = parent_msgs[..4]
            .iter()
            .map(|m| m.content.clone().unwrap())
            .collect();
        let child_contents: Vec<_> = child_msgs
            .iter()
            .map(|m| m.content.clone().unwrap())
            .collect();
        assert_eq!(parent_contents, child_contents);
    }

    #[test]
    fn fork_writes_independently() {
        let db = test_db();
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .unwrap();
        let anchor = db
            .insert_message("parent", "user", Some("q1"), None, None, None, None)
            .unwrap();

        db.fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .unwrap();
        db.insert_message("child", "user", Some("child-only"), None, None, None, None)
            .unwrap();

        let parent_msgs = db.load_session_messages("parent").unwrap();
        let child_msgs = db.load_session_messages("child").unwrap();
        assert_eq!(parent_msgs.len(), 1);
        assert_eq!(child_msgs.len(), 2);
        assert_eq!(child_msgs[1].content.as_deref(), Some("child-only"));
    }

    #[test]
    fn fork_lineage_persisted_on_session_row() {
        let db = test_db();
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .unwrap();
        let anchor = db
            .insert_message("parent", "user", Some("q1"), None, None, None, None)
            .unwrap();
        db.fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .unwrap();

        let row = db.session_by_id("child").unwrap().expect("child exists");
        assert_eq!(row.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(row.forked_from_message_id, Some(anchor));
        let parent_row = db.session_by_id("parent").unwrap().unwrap();
        assert_eq!(parent_row.parent_session_id, None);
        assert_eq!(parent_row.forked_from_message_id, None);
    }

    #[test]
    fn fork_invalid_anchor_errors_and_does_not_create_session() {
        let db = test_db();
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .unwrap();
        db.insert_message("parent", "user", Some("q1"), None, None, None, None)
            .unwrap();

        let bad = db.fork_session("parent", 999_999, "child", "fork", 200);
        assert!(bad.is_err());
        assert!(db.session_by_id("child").unwrap().is_none());
    }

    #[test]
    fn fork_unknown_parent_errors() {
        let db = test_db();
        let r = db.fork_session("ghost", 1, "child", "fork", 200);
        assert!(r.is_err());
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
