use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::db::Database;
use crate::security::validation::validate_session_id;
use crate::types::{Message, MessageContent, Role, ToolCall};

/// Lightweight metadata for a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Unique session identifier (UUID).
    pub id: String,
    /// Display title, auto-derived from first user message.
    pub title: String,
    /// RFC 3339 timestamp when the session was created.
    pub created_at: String,
    /// RFC 3339 timestamp of the last update.
    pub updated_at: String,
    /// Total number of messages in this session.
    pub message_count: usize,
}

/// A conversation session with metadata and message history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session metadata (id, title, timestamps).
    pub meta: SessionMeta,
    /// Ordered list of messages in this conversation.
    pub messages: Vec<Message>,
}

fn sessions_dir() -> Result<PathBuf> {
    let dir = Config::sessions_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// Create a new session with a fresh UUID and default title.
    pub fn new() -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Local::now().to_rfc3339();
        Self {
            meta: SessionMeta {
                id,
                title: "New conversation".to_string(),
                created_at: now.clone(),
                updated_at: now,
                message_count: 0,
            },
            messages: Vec::new(),
        }
    }

    /// Persist this session to disk as JSON (atomic write via temp file).
    pub fn save(&self) -> Result<()> {
        let path = session_path(&self.meta.id)?;
        let tmp_path = path.with_extension("json.tmp");
        let json = serde_json::to_string(self)?;
        // Write to temp file first, then rename for atomicity
        fs::write(&tmp_path, &json)?;
        fs::rename(&tmp_path, &path)?;

        Ok(())
    }

    /// Load a session from disk by its ID.
    pub fn load(id: &str) -> Result<Self> {
        let path = session_path(id)?;
        let json =
            fs::read_to_string(&path).with_context(|| format!("Session '{id}' not found"))?;
        let session: Session = serde_json::from_str(&json)?;
        Ok(session)
    }

    /// Update this session's messages and metadata from agent history.
    pub fn update_from_history(&mut self, history: &[Message]) {
        self.messages = history.to_vec();
        self.meta.message_count = history.len();
        self.meta.updated_at = Local::now().to_rfc3339();

        // Auto-title from first user message
        if self.meta.title == "New conversation" {
            if let Some(msg) = history.iter().find(|m| m.role == Role::User) {
                if let Some(content) = msg.text_content() {
                    // Use first paragraph only — appended metadata blocks
                    // (e.g. <proactive_nudges>) are separated by \n\n.
                    let first_para = content.split("\n\n").next().unwrap_or(content);
                    let title: String = first_para.chars().take(60).collect();
                    self.meta.title = if first_para.chars().count() > 60 {
                        format!("{title}...")
                    } else {
                        title
                    };
                }
            }
        }
    }
}

/// Fork an existing session at `anchor_message_id`, creating a new session
/// whose history is the parent's messages up to and including the anchor.
///
/// The fork is recorded in SQLite (parent lineage on the `sessions` row,
/// copied `messages` rows), then materialized as a JSON-on-disk session so
/// it shows up in `/sessions` and `Session::load` works without falling
/// through to `recover_session_from_db`.
///
/// Returns the new `Session`. Caller is responsible for switching the agent
/// to it (typically via `LoadSession`).
pub async fn fork_session(parent_id: &str, anchor_message_id: i64) -> Result<Session> {
    validate_session_id(parent_id)?;
    let db = Database::open().await?;
    let parent_row = db
        .session_by_id(parent_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Session '{parent_id}' not found"))?;

    let new_id = uuid::Uuid::new_v4().to_string();
    let now_secs = chrono::Utc::now().timestamp();
    let new_title = format!("{} (fork)", parent_row.title);
    db.fork_session(parent_id, anchor_message_id, &new_id, &new_title, now_secs)
        .await?;

    let messages = recover_session_from_db(&new_id).await?.unwrap_or_default();
    let now_iso = Local::now().to_rfc3339();
    let session = Session {
        meta: SessionMeta {
            id: new_id,
            title: new_title,
            created_at: now_iso.clone(),
            updated_at: now_iso,
            message_count: messages.len(),
        },
        messages,
    };
    session.save()?;
    Ok(session)
}

/// Load the most recently updated session, if any exist.
pub fn load_last_session() -> Result<Option<Session>> {
    let sessions = list_sessions()?;
    match sessions.first() {
        Some(meta) => match Session::load(&meta.id) {
            Ok(session) => Ok(Some(session)),
            Err(_) => Ok(None),
        },
        None => Ok(None),
    }
}

/// Lightweight struct for deserializing only the `meta` field from session files,
/// avoiding full deserialization of potentially large message arrays.
#[derive(Deserialize)]
struct SessionMetaOnly {
    meta: SessionMeta,
}

/// Attempt to recover a session's messages from SQLite if the JSON file is stale or missing.
pub async fn recover_session_from_db(session_id: &str) -> Result<Option<Vec<Message>>> {
    let db = Database::open().await?;
    let rows = db.load_session_messages(session_id).await?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut messages = Vec::with_capacity(rows.len());
    for row in rows {
        let role = match row.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => continue,
        };
        let tool_calls: Option<Vec<ToolCall>> = row
            .tool_calls_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok());
        let content = if let Some(parts_json) = &row.content_parts_json {
            match serde_json::from_str(parts_json) {
                Ok(parts) => Some(parts),
                Err(e) => {
                    tracing::warn!("Failed to deserialize content_parts_json: {e}");
                    row.content.map(MessageContent::Text)
                }
            }
        } else {
            row.content.map(MessageContent::Text)
        };
        messages.push(Message {
            role,
            content,
            tool_calls,
            tool_call_id: row.tool_call_id,
            timestamp: row.timestamp,
        });
    }

    Ok(Some(messages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_new_generates_valid_uuid_and_rfc3339_timestamps() {
        // The id must be a parseable UUID — the DB and gateway both index on
        // it, so an empty or malformed value would silently route to the
        // wrong session. Timestamps must parse as RFC3339 — that's the
        // persisted wire format and the only valid lex-sortable form.
        let session = Session::new();
        uuid::Uuid::parse_str(&session.meta.id)
            .expect("Session::new must produce a valid UUID for downstream indexing");
        chrono::DateTime::parse_from_rfc3339(&session.meta.created_at).expect(
            "Session::new must produce RFC3339 created_at — DB stores it as a string sort key",
        );
        chrono::DateTime::parse_from_rfc3339(&session.meta.updated_at)
            .expect("Session::new must produce RFC3339 updated_at");
    }

    #[test]
    fn update_from_history_sets_count_and_title() {
        let mut session = Session::new();
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text("Hello, world!".to_string())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.message_count, 1);
        assert_eq!(session.meta.title, "Hello, world!");
    }

    #[test]
    fn update_from_history_truncates_long_title() {
        let mut session = Session::new();
        let long_text = "a".repeat(100);
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text(long_text)),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert!(session.meta.title.ends_with("..."));
        assert!(session.meta.title.chars().count() <= 64); // 60 + "..."
    }

    #[test]
    fn update_from_history_skips_title_if_already_set() {
        let mut session = Session::new();
        session.meta.title = "Custom title".to_string();
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text("Different text".to_string())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        // Title should not change since it's not "New conversation"
        assert_eq!(session.meta.title, "Custom title");
    }

    #[test]
    fn update_from_history_ignores_assistant_for_title() {
        let mut session = Session::new();
        let messages = vec![Message {
            role: Role::Assistant,
            content: Some(MessageContent::Text("I am the assistant".to_string())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.title, "New conversation");
    }

    #[test]
    fn update_from_history_empty_preserves_title() {
        let mut session = Session::new();
        session.update_from_history(&[]);
        assert_eq!(session.meta.message_count, 0);
        assert_eq!(session.meta.title, "New conversation");
    }

    #[test]
    fn update_from_history_strips_proactive_nudges_from_title() {
        let mut session = Session::new();
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text(
                "*heartbeat tick*\n\n<proactive_nudges>\n- No messaging channels configured yet\n</proactive_nudges>".to_string(),
            )),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.title, "*heartbeat tick*");
    }

    #[test]
    fn update_from_history_strips_xml_blocks_from_title() {
        let mut session = Session::new();
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text(
                "Hello world\n\n<some_tag>\nstuff\n</some_tag>".to_string(),
            )),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.title, "Hello world");
    }

    #[test]
    fn update_from_history_preserves_single_paragraph_title() {
        let mut session = Session::new();
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text(
                "Simple question about code".to_string(),
            )),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.title, "Simple question about code");
    }

    #[test]
    fn update_from_history_first_paragraph_truncation() {
        let mut session = Session::new();
        let long_first = "A".repeat(80);
        let content = format!("{long_first}\n\n<proactive_nudges>\nstuff\n</proactive_nudges>");
        let messages = vec![Message {
            role: Role::User,
            content: Some(MessageContent::Text(content)),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }];
        session.update_from_history(&messages);
        assert!(session.meta.title.ends_with("..."));
        assert!(session.meta.title.chars().count() <= 64);
        assert!(!session.meta.title.contains("proactive_nudges"));
    }

    static SESSION_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_test_session(sessions_dir: &std::path::Path, id: &str, updated_at: &str) {
        let session = Session {
            meta: SessionMeta {
                id: id.to_string(),
                title: format!("Test {id}"),
                created_at: "2025-01-01T00:00:00+00:00".to_string(),
                updated_at: updated_at.to_string(),
                message_count: 1,
            },
            messages: vec![Message {
                role: Role::User,
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
            }],
        };
        let path = sessions_dir.join(format!("{id}.json"));
        fs::write(&path, serde_json::to_string(&session).unwrap()).unwrap();
    }

    #[test]
    fn load_last_session_returns_most_recently_updated() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        write_test_session(&sdir, "old-session", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "new-session", "2025-06-01T00:00:00+00:00");

        let result = load_last_session().unwrap().unwrap();
        assert_eq!(result.meta.id, "new-session");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn load_last_session_returns_none_when_no_sessions() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        let result = load_last_session().unwrap();
        assert!(result.is_none());
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn load_last_session_skips_corrupt_files() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        fs::write(sdir.join("corrupt.json"), "not valid json{{{").unwrap();
        write_test_session(&sdir, "valid-session", "2025-03-01T00:00:00+00:00");

        let result = load_last_session().unwrap().unwrap();
        assert_eq!(result.meta.id, "valid-session");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn save_then_load_last_session_roundtrip() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());

        let mut session = Session::new();
        session.meta.title = "Roundtrip test".to_string();
        session.save().unwrap();

        let loaded = load_last_session().unwrap().unwrap();
        assert_eq!(loaded.meta.id, session.meta.id);
        assert_eq!(loaded.meta.title, "Roundtrip test");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_empty() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let result = resolve_session_id("");
        assert!(matches!(result, Err(ResolveSessionError::Empty)));
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_not_found_empty_dir() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        let result = resolve_session_id("anything");
        assert!(matches!(result, Err(ResolveSessionError::NotFound(_))));
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_not_found() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");

        let result = resolve_session_id("zzz");
        match result {
            Err(ResolveSessionError::NotFound(p)) => assert_eq!(p, "zzz"),
            other => panic!("expected NotFound, got {other:?}"),
        }
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_exact_match() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");

        let meta = resolve_session_id("aaa-111").unwrap();
        assert_eq!(meta.id, "aaa-111");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_unique_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "bbb-222", "2025-02-01T00:00:00+00:00");

        let meta = resolve_session_id("aaa").unwrap();
        assert_eq!(meta.id, "aaa-111");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_ambiguous_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "aaa-222", "2025-02-01T00:00:00+00:00");

        let result = resolve_session_id("aaa");
        match result {
            Err(ResolveSessionError::Ambiguous { prefix, count }) => {
                assert_eq!(prefix, "aaa");
                assert_eq!(count, 2);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn resolve_session_id_exact_wins_over_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "abc", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "abcdef", "2025-02-01T00:00:00+00:00");

        // "abc" is both an exact id and a prefix of "abcdef" — exact must win.
        let meta = resolve_session_id("abc").unwrap();
        assert_eq!(meta.id, "abc");
        std::env::remove_var("BORG_DATA_DIR");
    }

    #[test]
    fn save_does_not_create_last_session_file() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_DATA_DIR", tmp.path());

        let session = Session::new();
        session.save().unwrap();

        let last_file = tmp.path().join("last_session");
        assert!(!last_file.exists());
        std::env::remove_var("BORG_DATA_DIR");
    }
}

/// Error returned when resolving a session ID prefix to a concrete session.
#[derive(Debug, thiserror::Error)]
pub enum ResolveSessionError {
    /// Empty prefix provided.
    #[error("Session ID must not be empty")]
    Empty,
    /// No session matched the prefix.
    #[error("No session found matching '{0}'")]
    NotFound(String),
    /// Multiple sessions matched the prefix.
    #[error("Ambiguous session ID '{prefix}' — matches {count} sessions")]
    Ambiguous {
        /// The prefix that was ambiguous.
        prefix: String,
        /// Number of matching sessions.
        count: usize,
    },
}

/// Resolve a full or prefix session ID to a concrete [`SessionMeta`].
///
/// Accepts exact matches and unique prefix matches. If the input exactly matches
/// a session ID, that session is returned even if it is also a prefix of longer IDs.
pub fn resolve_session_id(prefix: &str) -> std::result::Result<SessionMeta, ResolveSessionError> {
    if prefix.is_empty() {
        return Err(ResolveSessionError::Empty);
    }
    let sessions = list_sessions().unwrap_or_default();

    // Prefer exact match to disambiguate when a short id is also a prefix of longer ids.
    if let Some(exact) = sessions.iter().find(|s| s.id == prefix) {
        return Ok(exact.clone());
    }

    let matches: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|s| s.id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(ResolveSessionError::NotFound(prefix.to_string())),
        1 => Ok(matches[0].clone()),
        count => Err(ResolveSessionError::Ambiguous {
            prefix: prefix.to_string(),
            count,
        }),
    }
}

/// List all sessions sorted by most recently updated first.
pub fn list_sessions() -> Result<Vec<SessionMeta>> {
    let dir = sessions_dir()?;
    let mut sessions = Vec::new();

    if !dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(wrapper) = serde_json::from_str::<SessionMetaOnly>(&content) {
                    sessions.push(wrapper.meta);
                }
            }
        }
    }

    // Sort by updated_at descending
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}
