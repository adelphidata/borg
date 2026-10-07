use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::db::Database;
use crate::security::validation::validate_session_id;
use crate::types::{Message, Role};

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

/// Resolve the workspace identity of a directory for session stamping and
/// workspace-scoped resume: the symlink-canonicalized `cwd` plus the
/// (equally canonicalized) toplevel of its enclosing git checkout, if any.
///
/// Canonicalization keeps comparisons stable across symlinked paths (e.g.
/// macOS `/tmp` → `/private/tmp`); when it fails (dir vanished, permission)
/// the raw path is used rather than erroring — a best-guess stamp beats none.
/// The git toplevel is found by a pure filesystem walk (no `git` subprocess);
/// a linked `git worktree` checkout counts as its own workspace, which is
/// what workspace-scoped resume wants — worktrees are deliberately separate
/// working contexts.
pub fn workspace_identity(cwd: &std::path::Path) -> (PathBuf, Option<PathBuf>) {
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let repo_root =
        crate::git::find_repo_root(&canonical_cwd).map(|root| root.canonicalize().unwrap_or(root));
    (canonical_cwd, repo_root)
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

/// The title a session carries until its first user message names it.
pub const DEFAULT_TITLE: &str = "New conversation";

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
                title: DEFAULT_TITLE.to_string(),
                created_at: now.clone(),
                updated_at: now,
                message_count: 0,
            },
            messages: Vec::new(),
        }
    }

    /// Like [`Session::new`] but with a caller-chosen id instead of a random
    /// UUID. For daemon subsystems that need a *deterministic, resumable*
    /// session id (e.g. heartbeat remediation): state keyed on that id — the
    /// per-session goal — is only findable if the agent's `meta.id` actually
    /// equals the id, which requires `load_session(id)` to succeed, which
    /// requires the on-disk session to carry this id. The id must pass
    /// [`crate::security::validation::validate_session_id`] for `save` to work.
    pub fn new_with_id(id: String) -> Self {
        let mut session = Session::new();
        session.meta.id = id;
        session
    }

    /// Persist this session to disk as JSON.
    ///
    /// Goes through the shared atomic writer rather than a local temp+rename:
    /// that hand-rolled version named its temp file `<id>.json.tmp`, so two
    /// writers on the same session raced on one path, and it never fsynced, so
    /// a crash just after the rename could surface a zero-length transcript.
    pub fn save(&self) -> Result<()> {
        let path = session_path(&self.meta.id)?;
        let json = serde_json::to_string(self)?;
        coding::fs_atomic::atomic_write(&path, json.as_bytes())?;
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

        if self.meta.title == DEFAULT_TITLE {
            if let Some(title) = title_from_history(history) {
                self.meta.title = title;
            }
        }
    }
}

/// The placeholder title a session takes from its first user message: the
/// first paragraph, clipped to 60 characters. `None` before any user text.
pub fn title_from_history(history: &[Message]) -> Option<String> {
    let content = history
        .iter()
        .find(|m| m.role == Role::User)?
        .text_content()?;
    // Use first paragraph only — appended metadata blocks
    // (e.g. <proactive_nudges>) are separated by \n\n.
    let first_para = content.split("\n\n").next().unwrap_or(content);
    let title: String = first_para.chars().take(60).collect();
    Some(if first_para.chars().count() > 60 {
        format!("{title}...")
    } else {
        title
    })
}

/// Ensure a session with id `id` exists on disk, creating an empty one if it
/// doesn't. Idempotent: an existing session is left untouched (history
/// preserved). Used by subsystems that need a stable, pre-known session id so
/// `load_session(id)` succeeds on the first turn — without this,
/// `AgentBackend::new` mints a random id and any state keyed on `id` (e.g. a
/// goal) is never found again.
pub fn ensure_session_on_disk(id: &str) -> Result<()> {
    if Session::load(id).is_ok() {
        return Ok(());
    }
    Session::new_with_id(id.to_string()).save()
}

/// Fork an existing session at `anchor_message_id`, creating a new session
/// whose history is the parent's messages up to and including the anchor.
///
/// The fork is recorded in SQLite by reference only — parent lineage on the
/// child `sessions` row, no copied `messages` rows — then materialized as a
/// JSON-on-disk session (its reconstructed history) so it shows up in
/// `/sessions` and `Session::load` works without falling through to
/// `recover_session_from_db`.
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

/// Reconstruct a session's working history from the durable event log in
/// SQLite. The log is the source of truth; this is the canonical
/// "reconstruct from log" step — decode the persisted rows into
/// [`crate::conversation::SessionEvent`]s and project them into the
/// conversation the agent resumes from.
pub async fn recover_session_from_db(session_id: &str) -> Result<Option<Vec<Message>>> {
    let db = Database::open().await?;
    // Lineage-aware: a forked session inherits its ancestors' history by
    // reference (no rows are copied at fork time), so reconstruction stitches
    // the parent prefix in. For a non-forked session this is exactly its own
    // rows.
    let rows = db.load_session_lineage_rows(session_id).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    // Surface tampering/corruption of the durable log. Best-effort: a broken
    // chain still loads (we never strand the user out of their session) — it
    // just warns, which is what makes the HMAC chain's tamper-evidence live
    // rather than write-only.
    let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
    if !crate::conversation::verify_session_chain(&key, &rows) {
        tracing::warn!(
            "session {session_id}: message-log HMAC chain failed verification \
             (tampering or corruption); loading anyway"
        );
    }
    let events = crate::conversation::events_from_rows(rows);
    Ok(Some(crate::conversation::project_history(&events)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageContent;

    /// Workspace identity must find the git toplevel from any subdirectory
    /// and symlink-canonicalize both paths — otherwise a session stamped from
    /// `repo/sub` and a `--continue` issued from `repo/` (or through a
    /// symlinked temp path like macOS `/tmp`) would never compare equal.
    #[test]
    fn workspace_identity_finds_git_toplevel_and_canonicalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("deep").join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git must be runnable in tests");
        assert!(init.success(), "git init failed");

        let canonical_repo = repo.canonicalize().unwrap();
        let (cwd, root) = workspace_identity(&sub);
        assert_eq!(
            root.as_deref(),
            Some(canonical_repo.as_path()),
            "subdirectory must resolve to the canonical repo toplevel"
        );
        assert_eq!(
            cwd,
            sub.canonicalize().unwrap(),
            "cwd must be canonicalized"
        );

        // Same repo reached from its (possibly symlinked) raw tempdir path
        // must produce the identical root string — that equality is what the
        // DB resolver compares on.
        let (_, root_from_repo) = workspace_identity(&repo);
        assert_eq!(root_from_repo, root);

        // Outside any repo: no root, but still a canonical cwd.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let (cwd, root) = workspace_identity(&plain);
        assert_eq!(root, None, "non-repo directory must not claim a repo root");
        assert_eq!(cwd, plain.canonicalize().unwrap());
    }

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
            seq: None,
        }];
        session.update_from_history(&messages);
        assert_eq!(session.meta.message_count, 1);
        assert_eq!(session.meta.title, "Hello, world!");
    }

    /// Regression guard for the heartbeat-remediation session↔goal binding:
    /// `ensure_session_on_disk(id)` must persist a session whose `meta.id`
    /// equals `id` (so `AgentBackend`'s `load_session(id)` adopts that id
    /// instead of minting a random UUID — the bug that made goals keyed on
    /// `id` unfindable), and must be idempotent (never clobber existing
    /// history). Exercises a colon-bearing slug exactly like the real
    /// `heartbeat:scheduled` key.
    #[test]
    fn ensure_session_on_disk_materializes_resumable_deterministic_id() {
        // Must be the SAME mutex every other env-mutating test in this module
        // uses: a private lock here would let one of those tests clear
        // `BORG_HOME` mid-body and strand this test's tempdir (`save` then
        // fails with ENOENT).
        let _serial = SESSION_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os(crate::paths::BORG_HOME_ENV);
        // SAFETY: env mutation is serialized by LOCK and restored below.
        unsafe { std::env::set_var(crate::paths::BORG_HOME_ENV, tmp.path()) };

        let id = "heartbeat:scheduled";
        assert!(Session::load(id).is_err(), "precondition: no session yet");

        ensure_session_on_disk(id).expect("ensure must create the session");
        let loaded = Session::load(id).expect("session must exist after ensure");
        assert_eq!(
            loaded.meta.id, id,
            "persisted meta.id must equal the requested id, or load_session would mint a random one"
        );

        // Idempotent: a second ensure must preserve existing history.
        let mut populated = loaded;
        populated.messages.push(Message {
            role: Role::User,
            content: Some(MessageContent::Text("remember me".to_string())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
        });
        populated.save().expect("save populated session");
        ensure_session_on_disk(id).expect("second ensure is a no-op");
        assert_eq!(
            Session::load(id).expect("still exists").messages.len(),
            1,
            "existing history must be preserved across a redundant ensure"
        );

        // SAFETY: restore prior env under the same lock.
        unsafe {
            match prev {
                Some(v) => std::env::set_var(crate::paths::BORG_HOME_ENV, v),
                None => std::env::remove_var(crate::paths::BORG_HOME_ENV),
            }
        }
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
            seq: None,
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
            seq: None,
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
            seq: None,
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
            seq: None,
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
            seq: None,
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
            seq: None,
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
            seq: None,
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
                seq: None,
            }],
        };
        let path = sessions_dir.join(format!("{id}.json"));
        fs::write(&path, serde_json::to_string(&session).unwrap()).unwrap();
    }

    #[test]
    fn save_leaves_no_temp_file_for_the_session_loader_to_trip_over() {
        // The old hand-rolled save wrote `<id>.json.tmp` beside the target.
        // Two writers on one session raced on that single path, and a crash
        // between write and rename left the stray file in the sessions dir
        // that `load_last_session` scans. The shared atomic writer uses a
        // process- and call-unique temp name and removes it on every path.
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        let session = Session::new_with_id("save-residue".to_string());
        session.save().unwrap();

        let entries: Vec<String> = fs::read_dir(&sdir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["save-residue.json".to_string()],
            "save must leave only the transcript behind, got: {entries:?}"
        );

        // And the transcript must be readable back — a rename that landed on
        // the temp name instead would leave this unparseable.
        assert_eq!(
            Session::load("save-residue").unwrap().meta.id,
            "save-residue"
        );
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn load_last_session_returns_most_recently_updated() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        write_test_session(&sdir, "old-session", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "new-session", "2025-06-01T00:00:00+00:00");

        let result = load_last_session().unwrap().unwrap();
        assert_eq!(result.meta.id, "new-session");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn load_last_session_returns_none_when_no_sessions() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        let result = load_last_session().unwrap();
        assert!(result.is_none());
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn load_last_session_skips_corrupt_files() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        fs::write(sdir.join("corrupt.json"), "not valid json{{{").unwrap();
        write_test_session(&sdir, "valid-session", "2025-03-01T00:00:00+00:00");

        let result = load_last_session().unwrap().unwrap();
        assert_eq!(result.meta.id, "valid-session");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn save_then_load_last_session_roundtrip() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());

        let mut session = Session::new();
        session.meta.title = "Roundtrip test".to_string();
        session.save().unwrap();

        let loaded = load_last_session().unwrap().unwrap();
        assert_eq!(loaded.meta.id, session.meta.id);
        assert_eq!(loaded.meta.title, "Roundtrip test");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_empty() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let result = resolve_session_id("");
        assert!(matches!(result, Err(ResolveSessionError::Empty)));
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_not_found_empty_dir() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();

        let result = resolve_session_id("anything");
        assert!(matches!(result, Err(ResolveSessionError::NotFound(_))));
        std::env::remove_var("BORG_HOME");
    }

    /// Spec: an unreadable session store is reported as a listing failure,
    /// never as `NotFound` — the session may exist, and callers that start a
    /// fresh session on `NotFound` would silently abandon it.
    #[test]
    fn resolve_session_id_reports_unreadable_store_as_listing_error() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        // A regular file where the directory belongs: neither creatable nor
        // readable as a directory.
        fs::write(tmp.path().join("sessions"), b"not a directory").unwrap();

        let result = resolve_session_id("aaa");
        std::env::remove_var("BORG_HOME");
        match result {
            Err(ResolveSessionError::Listing(msg)) => assert!(!msg.is_empty()),
            other => panic!("expected Listing, got {other:?}"),
        }
    }

    #[test]
    fn resolve_session_id_not_found() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");

        let result = resolve_session_id("zzz");
        match result {
            Err(ResolveSessionError::NotFound(p)) => assert_eq!(p, "zzz"),
            other => panic!("expected NotFound, got {other:?}"),
        }
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_exact_match() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");

        let meta = resolve_session_id("aaa-111").unwrap();
        assert_eq!(meta.id, "aaa-111");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_unique_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "aaa-111", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "bbb-222", "2025-02-01T00:00:00+00:00");

        let meta = resolve_session_id("aaa").unwrap();
        assert_eq!(meta.id, "aaa-111");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_ambiguous_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
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
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn resolve_session_id_exact_wins_over_prefix() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());
        let sdir = tmp.path().join("sessions");
        fs::create_dir_all(&sdir).unwrap();
        write_test_session(&sdir, "abc", "2025-01-01T00:00:00+00:00");
        write_test_session(&sdir, "abcdef", "2025-02-01T00:00:00+00:00");

        // "abc" is both an exact id and a prefix of "abcdef" — exact must win.
        let meta = resolve_session_id("abc").unwrap();
        assert_eq!(meta.id, "abc");
        std::env::remove_var("BORG_HOME");
    }

    #[test]
    fn save_does_not_create_last_session_file() {
        let _lock = SESSION_ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("BORG_HOME", tmp.path());

        let session = Session::new();
        session.save().unwrap();

        let last_file = tmp.path().join("last_session");
        assert!(!last_file.exists());
        std::env::remove_var("BORG_HOME");
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
    /// The session directory could not be listed, so nothing is known about
    /// which sessions exist. Distinct from `NotFound`: the session may well be
    /// there, and callers must not treat it as missing.
    #[error("Could not list sessions: {0}")]
    Listing(String),
}

/// Resolve a full or prefix session ID to a concrete [`SessionMeta`].
///
/// Accepts exact matches and unique prefix matches. If the input exactly matches
/// a session ID, that session is returned even if it is also a prefix of longer IDs.
pub fn resolve_session_id(prefix: &str) -> std::result::Result<SessionMeta, ResolveSessionError> {
    if prefix.is_empty() {
        return Err(ResolveSessionError::Empty);
    }
    // A directory that cannot be read is an error in its own right, never an
    // empty listing: an unreadable store would otherwise make every id look
    // missing and send the caller down the "start fresh" path.
    let sessions = list_sessions().map_err(|e| ResolveSessionError::Listing(e.to_string()))?;

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

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}
