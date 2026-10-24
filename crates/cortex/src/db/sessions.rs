use anyhow::Result;
use libsql::params;
use std::collections::HashMap;
use tracing::{instrument, warn};

use super::models::{ChannelActivityRow, MessageRow, SessionLease, SessionRow};
use super::Database;

/// Hard cap on rows returned by [`Database::sessions_since`]. Acts as a
/// circuit-breaker against unbounded scans (e.g. heartbeat passes `since=0`).
/// Sized well above any realistic working-set so legitimate queries are not
/// silently truncated; if a deployment hits this we have a separate problem.
const SESSIONS_SINCE_HARD_CAP: i64 = 10_000;

/// A session worth resuming: not archived, not one of the daemon's own
/// check-in sessions (`heartbeat:*`, which every TUI start refreshes), and
/// holding a conversation — every TUI start writes an empty row, so an
/// open-and-quit must not shadow the conversation before it. A fork holds
/// its history through its parent, so it counts with no rows of its own.
const RESUMABLE_SESSION: &str = "archived_at IS NULL \
     AND id NOT LIKE 'heartbeat:%' \
     AND (parent_session_id IS NOT NULL \
          OR EXISTS (SELECT 1 FROM messages m WHERE m.session_id = sessions.id))";

/// One event for [`Database::append_session_events`]. Field-for-field the
/// parameters of [`Database::append_session_event`], owned so a batch can be
/// assembled before the transaction opens.
#[derive(Debug, Clone, Default)]
pub struct SessionEventDraft {
    /// Message role (`"user"`, `"assistant"`, `"tool"`, `"system"`).
    pub role: String,
    /// Fine-grained lifecycle tag; `None` for conversational messages.
    pub event_kind: Option<String>,
    /// Message text content.
    pub content: Option<String>,
    /// Serialized tool calls, when the event is an assistant tool-call turn.
    pub tool_calls_json: Option<String>,
    /// The tool call this event answers, for `role = "tool"` rows.
    pub tool_call_id: Option<String>,
    /// Caller-supplied wall-clock timestamp (RFC 3339).
    pub timestamp: Option<String>,
    /// Serialized multimodal content parts.
    pub content_parts_json: Option<String>,
    /// Lifecycle event payload, present iff `event_kind` is.
    pub event_json: Option<String>,
}

impl Database {
    /// Insert a session, or refresh an existing row's activity fields.
    ///
    /// Writing to a session un-archives it. Archiving keys on inactivity, so
    /// activity has to reverse it — otherwise resuming an archived session
    /// leaves it permanently hidden from listings despite fresh messages, and
    /// the sweep won't revisit it (it only considers `archived_at IS NULL`).
    /// A `pinned` session is never archived in the first place, so this can't
    /// disturb it.
    pub async fn upsert_session(
        &self,
        id: &str,
        created_at: i64,
        updated_at: i64,
        total_tokens: i64,
        model: &str,
        title: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sessions (id, created_at, updated_at, total_tokens, model, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                updated_at = ?3, total_tokens = ?4, model = ?5, title = ?6,
                archived_at = NULL",
                params!(id, created_at, updated_at, total_tokens, model, title),
            )
            .await?;
        Ok(())
    }

    /// Stamp a session's workspace identity: the canonicalized directory it
    /// was opened from and (when inside a git checkout) the repo toplevel.
    /// Last write wins — a session resumed from a different directory follows
    /// its user, keeping workspace-scoped resume anchored to where the
    /// session was most recently used. No-op when the row doesn't exist yet.
    pub async fn set_session_workspace(
        &self,
        id: &str,
        cwd: &str,
        git_repo_root: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE sessions SET cwd = ?1, git_repo_root = ?2 WHERE id = ?3",
                params!(cwd, git_repo_root, id),
            )
            .await?;
        Ok(())
    }

    /// Most recent resumable session belonging to the given workspace.
    ///
    /// Matching is two-tier: prefer sessions stamped with the same
    /// `git_repo_root` (any directory inside the same checkout counts as the
    /// same workspace), then fall back to an exact `cwd` match for
    /// directories outside any git repo. Returns `None` when neither tier
    /// matches — callers decide whether to fall back to the global MRU.
    /// Both arguments must be canonicalized the same way the stamps were
    /// (see `crate::session::workspace_identity`). Only conversations count:
    /// see [`RESUMABLE_SESSION`].
    pub async fn most_recent_session_in_workspace(
        &self,
        git_repo_root: Option<&str>,
        cwd: &str,
    ) -> Result<Option<SessionRow>> {
        if let Some(root) = git_repo_root {
            if let Some(row) = self
                .newest_resumable_session("AND git_repo_root = ?1", params!(root))
                .await?
            {
                return Ok(Some(row));
            }
        }
        self.newest_resumable_session("AND cwd = ?1", params!(cwd))
            .await
    }

    /// Most recent resumable session anywhere — `--continue`'s fallback when
    /// the workspace has none.
    pub async fn most_recent_resumable_session(&self) -> Result<Option<SessionRow>> {
        self.newest_resumable_session("", ()).await
    }

    async fn newest_resumable_session(
        &self,
        scope: &str,
        args: impl libsql::params::IntoParams,
    ) -> Result<Option<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                &format!(
                    "SELECT {} FROM sessions WHERE {RESUMABLE_SESSION} {scope} \
                     ORDER BY updated_at DESC, id DESC LIMIT 1",
                    SessionRow::COLUMNS
                ),
                args,
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(SessionRow::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Overwrite a session's title without touching token totals or model.
    /// Used by the aux-LLM title generator after the first user→assistant
    /// exchange. No-op when the session row hasn't been inserted yet.
    pub async fn update_session_title(&self, id: &str, title: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params!(title, now, id),
            )
            .await?;
        Ok(())
    }

    /// Create an untitled session row unless one already exists.
    ///
    /// For opening a session: a new one needs its row at once, but a resumed
    /// one keeps its title, its place in the list, and its archive state —
    /// opening a conversation is not activity in it.
    pub async fn ensure_session_row(&self, id: &str, created_at: i64, model: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sessions (id, created_at, updated_at, total_tokens, model, title)
             VALUES (?1, ?2, ?2, 0, ?3, '')
             ON CONFLICT(id) DO NOTHING",
                params!(id, created_at, model),
            )
            .await?;
        Ok(())
    }

    /// Record a finished turn on the session row: last activity, the model
    /// that answered, and a title when the row has none yet.
    ///
    /// The row is written once, empty-titled, when the session opens; without
    /// this every conversation lists as untitled and sorts by when it was
    /// opened rather than when it was last used. A title already on the row —
    /// the user's own, or the aux titler's — always wins, and an empty
    /// `title` leaves the row's alone. Activity un-archives, as in
    /// [`Self::upsert_session`]. No-op when the row does not exist.
    pub async fn record_session_turn(
        &self,
        id: &str,
        updated_at: i64,
        model: &str,
        title: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE sessions SET updated_at = ?2, model = ?3,
                    title = CASE WHEN title = '' THEN ?4 ELSE title END,
                    archived_at = NULL
                 WHERE id = ?1",
                params!(id, updated_at, model, title),
            )
            .await?;
        Ok(())
    }

    /// Name the rows among `ids` that have a conversation but no title, from
    /// their first user message, and return the titles written by id.
    ///
    /// A turn titles its own row, but rows from before that stayed untitled
    /// until their next turn — which an old conversation may never get — and
    /// listed as a column of identical "Untitled" rows. A title already on a
    /// row is never touched.
    pub async fn backfill_session_titles(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let placeholders = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let args: Vec<libsql::Value> = ids
            .iter()
            .map(|id| libsql::Value::Text(id.clone()))
            .collect();
        let mut rows = self
            .conn
            .query(
                &format!(
                    // One grouped pass for each session's first user
                    // message, not a subquery per message row.
                    "SELECT m.session_id, m.content FROM messages m \
                     JOIN sessions s ON s.id = m.session_id \
                     WHERE s.title = '' AND m.id IN ( \
                       SELECT MIN(id) FROM messages \
                       WHERE session_id IN ({placeholders}) AND role = 'user' \
                         AND event_kind IS NULL AND COALESCE(content, '') != '' \
                       GROUP BY session_id)"
                ),
                args,
            )
            .await?;
        let mut firsts = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            firsts.push((id, content));
        }
        for (id, content) in firsts {
            let Some(title) =
                crate::session::title_from_history(&[crate::types::Message::user(&content)])
            else {
                continue;
            };
            let title = title.trim().to_string();
            if title.is_empty() {
                continue;
            }
            // One row that cannot be written must not cost the others the
            // titles they already got.
            match self
                .conn
                .execute(
                    "UPDATE sessions SET title = ?2 WHERE id = ?1 AND title = ''",
                    params!(id.as_str(), title.as_str()),
                )
                .await
            {
                Ok(_) => {
                    out.insert(id, title);
                }
                Err(e) => tracing::warn!(session_id = %id, error = %e, "could not title session"),
            }
        }
        Ok(out)
    }

    /// Fetch a single session row by exact id. Returns `None` if absent.
    pub async fn session_by_id(&self, id: &str) -> Result<Option<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                &format!("SELECT {} FROM sessions WHERE id = ?1", SessionRow::COLUMNS),
                params!(id),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(SessionRow::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// List sessions by recency, always including pinned sessions.
    ///
    /// The recency window (`ORDER BY updated_at DESC LIMIT ?`) alone would let
    /// a pinned session fall off the end and become unreachable in the session
    /// picker — even though pinning promises the session is kept (it is exempt
    /// from retention purge and stale auto-archive). So a second bounded query
    /// appends any non-archived pinned rows the window missed, deduplicated.
    ///
    /// A pin never bypasses the archive filter: archived pinned rows are not
    /// resurrected by the catch-up (they stay reachable only the way any
    /// archived session is — inside the window, where callers filter on
    /// `archived_at` themselves).
    ///
    /// Ordering stays globally `updated_at DESC`: every row outside the top-
    /// `limit` window is by construction no newer than the window's oldest row,
    /// so appending the pinned extras (themselves recency-sorted) preserves the
    /// order. The result may exceed `limit` by the number of caught-up pinned
    /// rows, itself bounded by `limit`.
    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                &format!(
                    "SELECT {} FROM sessions ORDER BY updated_at DESC, id DESC LIMIT ?1",
                    SessionRow::COLUMNS
                ),
                params!(limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SessionRow::from_row(&row)?);
        }
        // `NOT IN (window)` in SQL rather than only deduping in Rust, so the
        // catch-up's own LIMIT budgets *missed* pinned rows — a window already
        // full of pinned sessions can't starve older pinned ones out of it.
        let mut pinned_rows = self
            .conn
            .query(
                &format!(
                    "SELECT {cols} FROM sessions \
                     WHERE pinned = 1 AND archived_at IS NULL \
                       AND id NOT IN \
                         (SELECT id FROM sessions ORDER BY updated_at DESC, id DESC LIMIT ?1) \
                     ORDER BY updated_at DESC, id DESC LIMIT ?1",
                    cols = SessionRow::COLUMNS
                ),
                params!(limit as i64),
            )
            .await?;
        while let Some(row) = pinned_rows.next().await? {
            let r = SessionRow::from_row(&row)?;
            if !out.iter().any(|s| s.id == r.id) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Return sessions updated since a given Unix timestamp, ordered by most recent first.
    ///
    /// Capped at [`SESSIONS_SINCE_HARD_CAP`] rows as a circuit-breaker. Callers
    /// like the heartbeat invoke this with `since=0` (= "all sessions"), which
    /// would otherwise scan the entire table for power users.
    pub async fn sessions_since(&self, since: i64) -> Result<Vec<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                &format!(
                    "SELECT {} FROM sessions WHERE updated_at >= ?1 \
                     ORDER BY updated_at DESC LIMIT ?2",
                    SessionRow::COLUMNS
                ),
                params!(since, SESSIONS_SINCE_HARD_CAP),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SessionRow::from_row(&row)?);
        }
        if out.len() == SESSIONS_SINCE_HARD_CAP as usize {
            // Hitting the cap means a future caller that walks the full set
            // will silently process a stale tail. Surface it so we notice
            // before someone relies on completeness.
            warn!(
                "sessions_since hit the {SESSIONS_SINCE_HARD_CAP}-row cap (since={since}); \
                 results are truncated"
            );
        }
        Ok(out)
    }

    /// Append a conversational message to the session event log. Thin wrapper
    /// over [`Database::append_session_event`] with no fine-grained event kind.
    #[instrument(skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_message(
        &self,
        session_id: &str,
        role: &str,
        content: Option<&str>,
        tool_calls_json: Option<&str>,
        tool_call_id: Option<&str>,
        timestamp: Option<&str>,
        content_parts_json: Option<&str>,
    ) -> Result<i64> {
        self.append_session_event(
            session_id,
            role,
            None,
            content,
            tool_calls_json,
            tool_call_id,
            timestamp,
            content_parts_json,
            None,
        )
        .await
    }

    /// Append one event to a session's append-only log, extending the
    /// per-session HMAC chain.
    ///
    /// This is the single-event form of [`Database::append_session_events`]:
    /// conversational messages (`event_kind = None`) and fine-grained
    /// lifecycle events (`event_kind = Some(tag)`, payload in `event_json`)
    /// both flow through here. Returns the new row's `id` (the event's `seq`).
    #[instrument(skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn append_session_event(
        &self,
        session_id: &str,
        role: &str,
        event_kind: Option<&str>,
        content: Option<&str>,
        tool_calls_json: Option<&str>,
        tool_call_id: Option<&str>,
        timestamp: Option<&str>,
        content_parts_json: Option<&str>,
        event_json: Option<&str>,
    ) -> Result<i64> {
        let draft = SessionEventDraft {
            role: role.to_string(),
            event_kind: event_kind.map(str::to_string),
            content: content.map(str::to_string),
            tool_calls_json: tool_calls_json.map(str::to_string),
            tool_call_id: tool_call_id.map(str::to_string),
            timestamp: timestamp.map(str::to_string),
            content_parts_json: content_parts_json.map(str::to_string),
            event_json: event_json.map(str::to_string),
        };
        let ids = self.append_session_events(session_id, &[draft]).await?;
        ids.first().copied().ok_or_else(|| {
            anyhow::anyhow!("append_session_events returned no id for a one-event batch")
        })
    }

    /// Append a batch of events to a session's append-only log in ONE
    /// transaction, extending the per-session HMAC chain link-by-link.
    ///
    /// This is the durable-append primitive. A turn that flushes an
    /// assistant message plus N tool results through the single-event form
    /// pays N+1 `BEGIN IMMEDIATE` transactions and N+1 chain-head reads;
    /// this reads the chain head once and computes the links in order, so
    /// the resulting rows are byte-identical to N+1 sequential appends.
    /// Mirrors `record_vitals_event`: `BEGIN IMMEDIATE` serializes the
    /// read-prev-then-write so two appenders can't fork the chain. Returns
    /// the new rows' `id`s (each event's `seq`) in input order.
    #[instrument(skip_all, fields(batch = drafts.len()))]
    pub async fn append_session_events(
        &self,
        session_id: &str,
        drafts: &[SessionEventDraft],
    ) -> Result<Vec<i64>> {
        if drafts.is_empty() {
            return Ok(Vec::new());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: Result<Vec<i64>> = async {
            let now = chrono::Utc::now().timestamp();
            // Per-session chain: link from the last event in THIS session, so
            // each session's log is its own chain and a fork starts fresh.
            // Last chained hmac in THIS session, or "0" when there is no prior
            // row.
            let mut prev_hmac: String = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT hmac FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                        params!(session_id),
                    )
                    .await?;
                let last = match rows.next().await? {
                    Some(row) => row.get::<Option<String>>(0)?,
                    None => None,
                };
                last.unwrap_or_else(|| "0".to_string())
            };
            let key = self.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
            let mut ids = Vec::with_capacity(drafts.len());
            for d in drafts {
                let hmac = crate::conversation::compute_session_event_hmac(
                    &key,
                    &prev_hmac,
                    &crate::conversation::SessionEventHashInput {
                        session_id,
                        role: &d.role,
                        event_kind: d.event_kind.as_deref(),
                        content: d.content.as_deref(),
                        tool_calls_json: d.tool_calls_json.as_deref(),
                        tool_call_id: d.tool_call_id.as_deref(),
                        timestamp: d.timestamp.as_deref(),
                        content_parts_json: d.content_parts_json.as_deref(),
                        event_json: d.event_json.as_deref(),
                        created_at: now,
                    },
                );
                self.conn.execute(
                    "INSERT INTO messages (session_id, role, content, tool_calls_json, tool_call_id,
                        timestamp, created_at, content_parts_json, event_kind, event_json, hmac, prev_hmac)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params!(
                        session_id, d.role.as_str(), d.content.as_deref(), d.tool_calls_json.as_deref(),
                        d.tool_call_id.as_deref(), d.timestamp.as_deref(), now,
                        d.content_parts_json.as_deref(), d.event_kind.as_deref(),
                        d.event_json.as_deref(), hmac.as_str(), prev_hmac.as_str()
                    ),
                ).await?;
                ids.push(self.conn.last_insert_rowid());
                prev_hmac = hmac;
            }
            Ok(ids)
        }
        .await;

        match result {
            Ok(ids) => {
                if let Err(commit_err) = self.conn.execute_batch("COMMIT").await {
                    // A failed COMMIT may leave the transaction open on this
                    // shared connection, wedging every later writer inside
                    // "cannot start a transaction within a transaction".
                    // Best-effort ROLLBACK clears that state; if the commit
                    // actually landed (fsync ambiguity) the rollback is a
                    // harmless no-op error.
                    if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                        tracing::warn!(
                            "sessions: rollback after failed append_session_events commit also failed: {re}"
                        );
                    }
                    return Err(commit_err.into());
                }
                Ok(ids)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("sessions: rollback after append_session_events failed: {re}");
                }
                Err(e)
            }
        }
    }

    /// Append a compaction checkpoint to a session's log: a non-destructive
    /// record that the events in `[from_seq, to_seq]` are summarized by
    /// `summary`. The raw rows are NOT deleted — [`crate::conversation::project_history`]
    /// folds the checkpoint in at read time, so the full history stays
    /// re-derivable, exportable, and forkable from before the compaction.
    ///
    /// This is the durable write behind compaction-as-event: the summary lands
    /// in the same append-only, HMAC-chained log as the messages it subsumes,
    /// rather than being spliced into the working history as a synthetic
    /// message. Returns the checkpoint event's `seq`.
    pub async fn append_compaction_checkpoint(
        &self,
        session_id: &str,
        from_seq: i64,
        to_seq: i64,
        summary: &str,
        model: &str,
    ) -> Result<i64> {
        let kind = crate::conversation::SessionEventKind::CompactionCheckpoint {
            from_seq,
            to_seq,
            summary: summary.to_string(),
            model: model.to_string(),
        };
        let event_json = serde_json::to_string(&kind)?;
        self.append_session_event(
            session_id,
            // Role is unused for fine-grained events (they decode from
            // `event_json`, not the conversational columns) but is part of the
            // hashed payload, so keep it stable.
            "system",
            Some(crate::conversation::COMPACTION_CHECKPOINT_KIND),
            None,
            None,
            None,
            None,
            None,
            Some(&event_json),
        )
        .await
    }

    pub async fn load_session_messages(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut rows = self.conn.query(
            "SELECT id, session_id, role, content, tool_calls_json, tool_call_id, timestamp, created_at, content_parts_json,
                    event_kind, event_json, hmac, prev_hmac
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
            params!(session_id),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(MessageRow {
                id: row.get::<i64>(0)?,
                session_id: row.get::<String>(1)?,
                role: row.get::<String>(2)?,
                content: row.get::<Option<String>>(3)?,
                tool_calls_json: row.get::<Option<String>>(4)?,
                tool_call_id: row.get::<Option<String>>(5)?,
                timestamp: row.get::<Option<String>>(6)?,
                created_at: row.get::<i64>(7)?,
                content_parts_json: row.get::<Option<String>>(8)?,
                event_kind: row.get::<Option<String>>(9)?,
                event_json: row.get::<Option<String>>(10)?,
                hmac: row.get::<Option<String>>(11)?,
                prev_hmac: row.get::<Option<String>>(12)?,
            });
        }
        Ok(out)
    }

    /// Load a session's full reconstructable history, following fork lineage.
    ///
    /// This is the read side of fork-by-reference. A forked session stores only
    /// the rows appended *after* the fork; its inherited history is never copied
    /// — it lives in the ancestors and is stitched in here. The walk goes from
    /// the session up to its root, then concatenates each ancestor's own rows
    /// truncated at the seq its descendant branched from (`ceiling`, the tightest
    /// anchor below it). Because `messages.id` is globally monotonic an ancestor's
    /// rows always precede its descendants', so the concatenation is already in
    /// `seq` order — and because absolute `seq`s are preserved (not remapped on a
    /// copy), a `CompactionCheckpoint` inherited from an ancestor still references
    /// the right rows. For a non-forked session this returns exactly
    /// [`Self::load_session_messages`].
    pub async fn load_session_lineage_rows(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        // Walk child -> root. `ceiling` is the highest seq visible at this level:
        // i64::MAX for the leaf (all its own rows), then narrowed to the tightest
        // anchor seen below as we climb. A `visited` set bounds malformed cycles.
        let mut segments: Vec<(String, i64)> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut current = session_id.to_string();
        let mut ceiling = i64::MAX;
        while visited.insert(current.clone()) {
            let Some(meta) = self.session_by_id(&current).await? else {
                break; // unknown ancestor — stitch what we have
            };
            segments.push((current.clone(), ceiling));
            match (meta.parent_session_id, meta.forked_from_message_id) {
                (Some(parent), Some(anchor)) => {
                    ceiling = ceiling.min(anchor);
                    current = parent;
                }
                _ => break, // reached a root
            }
        }
        // segments is leaf..root; assemble root..leaf so rows come out seq-ascending.
        let mut out = Vec::new();
        for (sid, ceiling) in segments.into_iter().rev() {
            let rows = self.load_session_messages(&sid).await?;
            out.extend(rows.into_iter().filter(|r| r.id <= ceiling));
        }
        Ok(out)
    }

    /// Fork a session at `anchor_message_id`: create a new child session row
    /// that references `parent_id` by lineage (`parent_session_id` +
    /// `forked_from_message_id`) and copies **no** message rows. The child's
    /// inherited history is reconstructed on demand by
    /// [`Self::load_session_lineage_rows`].
    ///
    /// Reference-by-offset (rather than the old deep-copy) is what makes
    /// forking fall out of the log: branching is O(1), multiple branches from one
    /// offset are free, and — because absolute `seq`s are preserved — a
    /// `CompactionCheckpoint` in the parent's history survives the fork intact
    /// (a deep copy would remap the ids the checkpoint references). Returns the
    /// number of messages the child inherits. Errors if the anchor is not part of
    /// the parent's reconstructable history, or the parent row is missing.
    pub async fn fork_session(
        &self,
        parent_id: &str,
        anchor_message_id: i64,
        new_id: &str,
        new_title: &str,
        now: i64,
    ) -> Result<usize> {
        let parent = self
            .session_by_id(parent_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("parent session '{parent_id}' not found"))?;
        // The anchor must be a real seq in the parent's reconstructable history
        // (which may itself be inherited, if the parent is a fork).
        let parent_history = self.load_session_lineage_rows(parent_id).await?;
        if !parent_history.iter().any(|r| r.id == anchor_message_id) {
            anyhow::bail!("anchor message {anchor_message_id} not found in session '{parent_id}'");
        }
        let inherited = parent_history
            .iter()
            .filter(|r| r.id <= anchor_message_id)
            .count();
        self.conn
            .execute(
                "INSERT INTO sessions
                (id, created_at, updated_at, total_tokens, model, title,
                 parent_session_id, forked_from_message_id)
             VALUES (?1, ?2, ?2, 0, ?3, ?4, ?5, ?6)",
                params!(
                    new_id,
                    now,
                    parent.model,
                    new_title,
                    parent_id,
                    anchor_message_id,
                ),
            )
            .await?;
        Ok(inherited)
    }

    // ── Per-session lease (worker-pool / fallible executors) ──
    //
    // The lease is what lets "any worker resumes any session" fall out of the
    // log: a worker takes the lease, reconstructs the session from the log
    // (`load_session_lineage_rows` → `project_history`), advances one step,
    // appends, and releases. If it dies mid-turn the lease expires and another
    // worker repeats the cycle from the durable log. With a single worker the
    // lease is always free or self-held, so the claim is re-entrant and the
    // existing single-process behavior is unchanged. Mirrors the proven
    // `claim_due_tasks` lease shape (`BEGIN IMMEDIATE` serializes the
    // read-then-write so two workers can't both win a free lease).

    /// Take (or re-take) the lease on a session. Succeeds when the lease is
    /// free, already expired, or already held by `worker_id` (re-entrant — the
    /// single-process case). A takeover from a different or expired owner bumps
    /// `lease_epoch` so the previous holder is fenced out of any later
    /// `renew`/`release`. Returns the granted lease, or `None` when another
    /// worker holds a still-live lease (or the session row doesn't exist).
    pub async fn claim_session_lease(
        &self,
        session_id: &str,
        worker_id: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<Option<SessionLease>> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: Result<Option<SessionLease>> = async {
            let mut rows = self
                .conn
                .query(
                    "SELECT lease_owner, lease_expiry, lease_epoch FROM sessions WHERE id = ?1",
                    params!(session_id),
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(None); // unknown session — caller must upsert first
            };
            let owner: Option<String> = row.get(0)?;
            let expiry: Option<i64> = row.get(1)?;
            let epoch: i64 = row.get(2)?;
            drop(rows);

            let reentrant = owner.as_deref() == Some(worker_id);
            let free = match (&owner, expiry) {
                (None, _) => true,
                _ if reentrant => true,
                (Some(_), Some(exp)) => exp <= now,
                (Some(_), None) => true, // owner with no deadline — defensive: treat as free
            };
            if !free {
                return Ok(None);
            }
            // Keep the epoch stable on a re-entrant renew (so an existing handle
            // stays valid); advance it on a takeover to fence the old owner.
            let new_epoch = if reentrant { epoch } else { epoch + 1 };
            let new_expiry = now + ttl_secs;
            self.conn
                .execute(
                    "UPDATE sessions SET lease_owner = ?1, lease_expiry = ?2, lease_epoch = ?3 \
                     WHERE id = ?4",
                    params!(worker_id, new_expiry, new_epoch, session_id),
                )
                .await?;
            Ok(Some(SessionLease {
                session_id: session_id.to_string(),
                owner: worker_id.to_string(),
                epoch: new_epoch,
                expiry: new_expiry,
            }))
        }
        .await;

        self.finish_immediate_tx(result, "sessions claim_session_lease")
            .await
    }

    /// Extend a held lease's deadline. Returns `false` if the lease was
    /// reclaimed (epoch advanced) or taken by another owner — the caller must
    /// stop writing, as it no longer owns the session.
    pub async fn renew_session_lease(
        &self,
        session_id: &str,
        worker_id: &str,
        epoch: i64,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET lease_expiry = ?1 \
                 WHERE id = ?2 AND lease_owner = ?3 AND lease_epoch = ?4",
                params!(now + ttl_secs, session_id, worker_id, epoch),
            )
            .await?;
        Ok(updated == 1)
    }

    /// Release a held lease (idempotent). Only the matching owner+epoch clears
    /// it, so a fenced (stale-epoch) worker calling release is a no-op.
    pub async fn release_session_lease(
        &self,
        session_id: &str,
        worker_id: &str,
        epoch: i64,
    ) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET lease_owner = NULL, lease_expiry = NULL \
                 WHERE id = ?1 AND lease_owner = ?2 AND lease_epoch = ?3",
                params!(session_id, worker_id, epoch),
            )
            .await?;
        Ok(updated == 1)
    }

    /// Every session id starting with `prefix`.
    ///
    /// Its own query rather than filtering a `list_sessions` page: the CLI
    /// resolves short id prefixes, and a paged listing would silently miss a
    /// match that fell past the limit — resolving to "not found", or worse to
    /// the wrong session because the real one wasn't in the page.
    pub async fn session_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        // Escape LIKE wildcards so a literal `%` or `_` in the prefix matches
        // itself instead of acting as a pattern.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM sessions WHERE id LIKE ?1 ESCAPE '\\' ORDER BY updated_at DESC",
                params!(format!("{escaped}%")),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row.get::<String>(0)?);
        }
        Ok(out)
    }

    /// Pin (or unpin) a session. Returns false when no such session exists.
    ///
    /// A pinned session is exempt from the retention purge and from the stale
    /// auto-archive sweep — the per-session counterpart to the daemon-wide
    /// `data.purge_policy = Never`.
    pub async fn set_session_pinned(&self, id: &str, pinned: bool) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET pinned = ?1 WHERE id = ?2",
                params!(pinned as i64, id),
            )
            .await?;
        Ok(updated > 0)
    }

    /// Soft-hide sessions with no activity since `cutoff`. Returns how many
    /// were archived.
    ///
    /// Ages on `updated_at`, not `created_at`: a conversation started a year
    /// ago and used yesterday is active, and archiving it by birthday would be
    /// the wrong answer. Never deletes — archiving only sets a timestamp, so
    /// it is always reversible — and never touches a pinned or
    /// already-archived session.
    pub async fn archive_stale_sessions(&self, cutoff: i64, now: i64) -> Result<usize> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET archived_at = ?1 \
                 WHERE updated_at < ?2 AND pinned = 0 AND archived_at IS NULL",
                params!(now, cutoff),
            )
            .await?;
        Ok(updated as usize)
    }

    /// Archive or restore one session on demand. Returns false when no such
    /// session exists.
    ///
    /// The per-session counterpart to [`Self::archive_stale_sessions`]: the
    /// sweep hides conversations by age, this hides the one the user closed.
    /// Like the sweep it never deletes — `Some(now)` stamps the close, `None`
    /// clears it — so a closed conversation stays recoverable.
    ///
    /// Unlike the sweep it ignores `pinned` and re-stamps an already-archived
    /// row: both guards exist to keep *automatic* archiving from surprising the
    /// user, and neither should override an explicit request.
    pub async fn set_session_archived(&self, id: &str, archived_at: Option<i64>) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET archived_at = ?1 WHERE id = ?2",
                params!(archived_at, id),
            )
            .await?;
        Ok(updated > 0)
    }

    /// Clear a session's archived flag, bringing it back into listings.
    /// Returns false when no such session exists.
    pub async fn unarchive_session(&self, id: &str) -> Result<bool> {
        self.set_session_archived(id, None).await
    }

    /// Free every lease whose deadline has passed, relative to `now`. This is
    /// the TTL-based runtime reclaim: a live worker can take over a session
    /// whose previous holder died more than one TTL ago. Returns the number of
    /// leases reclaimed.
    pub async fn expire_stale_session_leases(&self, now: i64) -> Result<u64> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET lease_owner = NULL, lease_expiry = NULL \
                 WHERE lease_owner IS NOT NULL AND lease_expiry IS NOT NULL AND lease_expiry <= ?1",
                params!(now),
            )
            .await?;
        Ok(updated)
    }

    /// Reclaim *every* session lease unconditionally. Run once at daemon boot:
    /// a cold start means no prior in-process lease holder can still be alive
    /// (the worker id is per-process, so a restarted daemon would otherwise be
    /// unable to recognise — and reclaim — leases its own previous instance
    /// left behind, locking those sessions until the TTL lapsed). Clearing the
    /// owner while leaving `lease_epoch` intact still fences any zombie writer,
    /// because the next claim advances the epoch. Returns the number reclaimed.
    pub async fn reclaim_all_session_leases(&self) -> Result<u64> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET lease_owner = NULL, lease_expiry = NULL \
                 WHERE lease_owner IS NOT NULL",
                params!(),
            )
            .await?;
        Ok(updated)
    }

    pub async fn delete_session_messages(&self, session_id: &str) -> Result<usize> {
        let count = self
            .conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params!(session_id),
            )
            .await?;
        Ok(count as usize)
    }

    /// Look up an existing channel session WITHOUT creating one. Returns
    /// `Ok(None)` when no row exists yet — used by read-only paths like
    /// `/btw` that need the current transcript snapshot but must not
    /// materialise a new session as a side effect.
    pub async fn lookup_channel_session(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<Option<String>> {
        let mut rows = self.conn.query(
            "SELECT session_id FROM channel_sessions WHERE channel_name = ?1 AND sender_id = ?2",
            params!(channel_name, sender_id),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }

    pub async fn resolve_channel_session(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        let existing: Option<String> = {
            let mut rows = self.conn.query(
                "SELECT session_id FROM channel_sessions WHERE channel_name = ?1 AND sender_id = ?2",
                params!(channel_name, sender_id),
            ).await?;
            match rows.next().await? {
                Some(row) => Some(row.get::<String>(0)?),
                None => None,
            }
        };

        match existing {
            Some(session_id) => {
                self.conn.execute(
                    "UPDATE channel_sessions SET last_active = ?1 WHERE channel_name = ?2 AND sender_id = ?3",
                    params!(now, channel_name, sender_id),
                ).await?;
                Ok(session_id)
            }
            None => {
                let session_id = uuid::Uuid::new_v4().to_string();
                self.conn.execute(
                    "INSERT INTO channel_sessions (channel_name, sender_id, session_id, created_at, last_active)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params!(channel_name, sender_id, session_id.clone(), now, now),
                ).await?;
                Ok(session_id)
            }
        }
    }

    /// Point the chat `(channel_name, sender_id)` at `session_id`, so its
    /// next turns continue that session. Used when a handoff lands here.
    pub async fn bind_channel_session(
        &self,
        channel_name: &str,
        sender_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO channel_sessions (channel_name, sender_id, session_id, created_at, last_active)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(channel_name, sender_id)
                 DO UPDATE SET session_id = excluded.session_id, last_active = excluded.last_active",
                params!(channel_name, sender_id, session_id, now),
            )
            .await?;
        Ok(())
    }

    pub async fn log_channel_message(
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
            params!(channel_name, sender_id, direction, content, metadata_json, session_id, now),
        ).await?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update session_id for a channel session (used by /new command).
    pub async fn update_channel_session_id(
        &self,
        channel_name: &str,
        sender_id: &str,
        new_session_id: &str,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let updated = self.conn.execute(
            "UPDATE channel_sessions SET session_id = ?1, last_active = ?2 WHERE channel_name = ?3 AND sender_id = ?4",
            params!(new_session_id, now, channel_name, sender_id),
        ).await?;
        Ok(updated > 0)
    }

    /// Message counts for many sessions in one query.
    ///
    /// The per-session [`Self::count_session_messages`] is fine for one row and
    /// ruinous for a listing: a hundred sessions meant a hundred round trips
    /// before a listing could be rendered. Sessions with no messages are absent
    /// from the map rather than present as zero.
    pub async fn count_session_messages_batch(
        &self,
        session_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        let mut out = HashMap::new();
        if session_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = (1..=session_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let args: Vec<libsql::Value> = session_ids
            .iter()
            .map(|id| libsql::Value::Text(id.clone()))
            .collect();
        let mut rows = self
            .conn
            .query(
                &format!(
                    "SELECT session_id, COUNT(*) FROM messages \
                     WHERE session_id IN ({placeholders}) GROUP BY session_id"
                ),
                args,
            )
            .await?;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            out.insert(id, count.max(0) as usize);
        }
        Ok(out)
    }

    /// The newest conversational message of each of many sessions, as
    /// `(role, content)`.
    ///
    /// Only rows a person said or read: `event_kind` rows are lifecycle
    /// bookkeeping (compaction checkpoints, fork markers) and tool results are
    /// machine output, and either one rendered into a conversation list reads
    /// as leaked JSON. A session whose only rows are those is simply absent.
    pub async fn last_message_previews(
        &self,
        session_ids: &[String],
    ) -> Result<HashMap<String, (String, String)>> {
        let mut out = HashMap::new();
        if session_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = (1..=session_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let args: Vec<libsql::Value> = session_ids
            .iter()
            .map(|id| libsql::Value::Text(id.clone()))
            .collect();
        // The inner query picks one id per session — the highest, which is the
        // newest since `messages.id` is an autoincrement — and the outer one
        // reads those rows. Filtering in both halves matters: filtering only
        // outside would pick the newest row and then discard it when it turned
        // out to be a tool result, leaving the session with no preview at all.
        let predicate = "session_id IN ({placeholders}) AND event_kind IS NULL \
             AND content IS NOT NULL AND TRIM(content) <> '' \
             AND role IN ('user', 'assistant')";
        let predicate = predicate.replace("{placeholders}", &placeholders);
        let mut rows = self
            .conn
            .query(
                &format!(
                    "SELECT session_id, role, content FROM messages \
                     WHERE {predicate} \
                       AND id IN (SELECT MAX(id) FROM messages WHERE {predicate} \
                                  GROUP BY session_id)"
                ),
                [args.clone(), args].concat(),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let role: String = row.get(1)?;
            let content: String = row.get(2)?;
            out.insert(id, (role, content));
        }
        Ok(out)
    }

    /// Count messages in a session.
    pub async fn count_session_messages(&self, session_id: &str) -> Result<usize> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params!(session_id),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as usize)
    }

    /// Count sessions whose `updated_at` is at or after `since_ts`. A session
    /// counts as "active in window" if it was touched, not merely created
    /// before the window — matches the wording in `/usage`.
    pub async fn session_count_since(&self, since_ts: i64) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM sessions WHERE updated_at >= ?1",
                params!(since_ts),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as u64)
    }

    /// Count messages whose `created_at` is at or after `since_ts`.
    pub async fn message_count_since(&self, since_ts: i64) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM messages WHERE created_at >= ?1",
                params!(since_ts),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as u64)
    }

    /// Aggregate tool-call counts across `messages.tool_calls_json` for rows
    /// at or after `since_ts`, returning up to `top_n` `(name, count)` pairs
    /// sorted by count descending.
    ///
    /// Two on-disk shapes are tolerated:
    /// - OpenAI-style: `[{ "function": { "name": "..." }, ... }, ...]`
    /// - Anthropic-style assistant tool blocks: `[{ "name": "...", "input": {...} }, ...]`
    ///
    /// Rows whose `tool_calls_json` fails to parse are skipped with a
    /// `tracing::warn!` and do not abort the aggregation — `/usage` must
    /// not fail if one corrupted row exists somewhere in history.
    #[instrument(skip_all)]
    pub async fn tool_call_counts_since(
        &self,
        since_ts: i64,
        top_n: usize,
    ) -> Result<Vec<(String, u64)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT tool_calls_json FROM messages
             WHERE created_at >= ?1 AND tool_calls_json IS NOT NULL",
                params!(since_ts),
            )
            .await?;

        let mut counts: HashMap<String, u64> = HashMap::new();
        while let Some(row) = rows.next().await? {
            let json = match row.get::<String>(0) {
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
    pub async fn channel_activity_since(&self, since_ts: i64) -> Result<Vec<ChannelActivityRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT channel_name, COUNT(*) AS msg_count, MAX(created_at) AS last_active
             FROM channel_messages
             WHERE created_at >= ?1
             GROUP BY channel_name
             ORDER BY msg_count DESC, channel_name ASC",
                params!(since_ts),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let count: i64 = row.get::<i64>(1)?;
            out.push(ChannelActivityRow {
                channel_name: row.get::<String>(0)?,
                message_count: count.max(0) as u64,
                last_active_unix: row.get::<i64>(2)?,
            });
        }
        Ok(out)
    }

    /// Keep the last `keep` messages in a session, deleting older ones.
    /// Returns the number of deleted messages.
    pub async fn compact_session_messages(&self, session_id: &str, keep: usize) -> Result<usize> {
        let count = self
            .conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1 AND id NOT IN \
             (SELECT id FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2)",
                params!(session_id, keep as i64),
            )
            .await?;
        Ok(count as usize)
    }

    /// Delete the last assistant turn (assistant + tool messages) from a session.
    /// Walks backwards from the end, collecting messages until a `user` message is
    /// hit. Returns the number of deleted messages.
    pub async fn delete_last_assistant_turn(&self, session_id: &str) -> Result<usize> {
        let messages = self.load_session_messages(session_id).await?;
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
        // Message ids are i64 from SQLite ROWIDs — safe to embed in SQL.
        let id_list: String = ids_to_delete
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM messages WHERE session_id = ?1 AND id IN ({id_list})");
        let count = self.conn.execute(&sql, params!(session_id)).await?;
        Ok(count as usize)
    }

    /// Full-text candidate search over past session messages. Returns rows
    /// matching the sanitized query with `score = 0.0` — BM25 ranking is
    /// computed in Rust by the caller via [`crate::memory::bm25`] so the
    /// vector + lexical fusion stays in one place rather than being split
    /// across SQL and Rust.
    ///
    /// `limit` is the *final* result count the caller intends to surface;
    /// this method oversamples by 4× (capped at 200) so the BM25 re-rank has
    /// enough breadth to be meaningful.
    pub async fn messages_fts_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MessageFtsHit>> {
        let sanitized = sanitize_messages_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let candidate_limit = limit.saturating_mul(4).min(200).max(limit);
        let mut rows = self
            .conn
            .query(
                "SELECT m.id, m.session_id, m.role, m.content, m.created_at
                 FROM messages m
                 JOIN messages_fts f ON m.id = f.rowid
                 WHERE messages_fts MATCH ?1
                 LIMIT ?2",
                params!(sanitized, candidate_limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(MessageFtsHit {
                id: row.get::<i64>(0)?,
                session_id: row.get::<String>(1)?,
                role: row.get::<String>(2)?,
                content: row.get::<Option<String>>(3)?.unwrap_or_default(),
                created_at: row.get::<i64>(4)?,
                score: 0.0,
            });
        }
        Ok(out)
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

    async fn test_db() -> Database {
        Database::test_db().await
    }

    /// A listing renders one line per session, and that line must come from
    /// something a person said. A tool result or a compaction checkpoint
    /// rendered there reads as leaked JSON, and the newest row is very often
    /// one of those — so picking the newest row and filtering afterwards would
    /// leave the busiest sessions with no preview at all.
    #[tokio::test]
    async fn last_message_previews_skip_tool_and_lifecycle_rows() {
        let db = test_db().await;
        for id in ["chatty", "quiet", "tools-only"] {
            db.upsert_session(id, 1, 10, 0, "m", "")
                .await
                .expect("seed");
        }
        db.insert_message("chatty", "user", Some("first"), None, None, None, None)
            .await
            .expect("user row");
        db.insert_message(
            "chatty",
            "assistant",
            Some("second"),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("assistant row");
        // Newest row in the session, and exactly what must not be quoted.
        db.insert_message(
            "chatty",
            "tool",
            Some("{\"ok\":true}"),
            None,
            Some("c1"),
            None,
            None,
        )
        .await
        .expect("tool row");
        db.append_session_event(
            "chatty",
            "assistant",
            Some("compaction"),
            None,
            None,
            None,
            None,
            None,
            Some("{}"),
        )
        .await
        .expect("event row");
        db.insert_message(
            "tools-only",
            "tool",
            Some("output"),
            None,
            Some("c2"),
            None,
            None,
        )
        .await
        .expect("tool row");

        let previews = db
            .last_message_previews(&[
                "chatty".to_string(),
                "quiet".to_string(),
                "tools-only".to_string(),
            ])
            .await
            .expect("previews");

        assert_eq!(
            previews.get("chatty"),
            Some(&("assistant".to_string(), "second".to_string())),
            "the newest thing a person would recognise, not the newest row"
        );
        assert!(
            !previews.contains_key("quiet"),
            "a session with no messages has nothing to preview"
        );
        assert!(
            !previews.contains_key("tools-only"),
            "a session of pure tool output must be absent, not quoted"
        );
    }

    /// The listing hot path counts messages for every row it returns. One
    /// query per row meant a hundred round trips before a list could render.
    #[tokio::test]
    async fn count_session_messages_batch_matches_the_per_session_count() {
        let db = test_db().await;
        for id in ["a", "b", "empty"] {
            db.upsert_session(id, 1, 10, 0, "m", "")
                .await
                .expect("seed");
        }
        for _ in 0..3 {
            db.insert_message("a", "user", Some("x"), None, None, None, None)
                .await
                .expect("row");
        }
        db.insert_message("b", "user", Some("y"), None, None, None, None)
            .await
            .expect("row");

        let counts = db
            .count_session_messages_batch(&["a".to_string(), "b".to_string(), "empty".to_string()])
            .await
            .expect("counts");

        assert_eq!(
            counts.get("a").copied(),
            Some(db.count_session_messages("a").await.unwrap())
        );
        assert_eq!(counts.get("b").copied(), Some(1));
        assert!(
            !counts.contains_key("empty"),
            "a session with no messages is absent rather than zero"
        );
        assert!(
            db.count_session_messages_batch(&[])
                .await
                .expect("empty")
                .is_empty(),
            "an empty listing must not build a query with no placeholders"
        );
    }

    /// The workspace resolver must scope by git repo root first (any cwd
    /// inside the same checkout counts), fall back to an exact cwd match for
    /// non-repo directories, skip archived rows, and honor recency within a
    /// workspace — otherwise `--continue` in repo A can resume a repo-B (or
    /// dead) session.
    #[tokio::test]
    async fn workspace_resolver_scopes_by_repo_then_cwd_and_skips_archived() {
        let db = test_db().await;
        // (id, updated_at, cwd, repo_root, archived)
        let rows: &[(&str, i64, &str, Option<&str>, bool)] = &[
            ("a-old", 10, "/w/a/sub", Some("/w/a"), false),
            ("a-new", 20, "/w/a", Some("/w/a"), false),
            ("a-dead", 30, "/w/a", Some("/w/a"), true), // newest in A but archived
            ("b", 40, "/w/b", Some("/w/b"), false),
            ("nogit", 50, "/w/plain", None, false),
        ];
        for (id, updated_at, cwd, root, archived) in rows {
            db.upsert_session(id, 1, *updated_at, 0, "m", "")
                .await
                .unwrap();
            db.insert_message(id, "user", Some("hi"), None, None, None, None)
                .await
                .unwrap();
            db.set_session_workspace(id, cwd, *root).await.unwrap();
            if *archived {
                db.conn
                    .execute(
                        "UPDATE sessions SET archived_at = 99 WHERE id = ?1",
                        params!(*id),
                    )
                    .await
                    .unwrap();
            }
        }

        // Repo tier: newest *non-archived* session of repo A wins, even though
        // an archived A-session and other-workspace sessions are more recent.
        let hit = db
            .most_recent_session_in_workspace(Some("/w/a"), "/w/a/somewhere-else")
            .await
            .unwrap()
            .expect("repo A has live sessions");
        assert_eq!(
            hit.id, "a-new",
            "repo tier must be recency-ordered and skip archived"
        );

        // Repo isolation: repo B resolves to its own session, never A's.
        let hit = db
            .most_recent_session_in_workspace(Some("/w/b"), "/w/b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, "b");

        // cwd tier: outside any repo, an exact cwd match resolves.
        let hit = db
            .most_recent_session_in_workspace(None, "/w/plain")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, "nogit");

        // Unknown repo root falls through to the cwd tier rather than
        // grabbing some other workspace's session.
        let hit = db
            .most_recent_session_in_workspace(Some("/w/unknown"), "/w/plain")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, "nogit", "repo miss must fall back to cwd match");

        // Nothing matches either tier → None (caller decides global fallback).
        assert!(db
            .most_recent_session_in_workspace(Some("/w/nowhere"), "/w/nowhere")
            .await
            .unwrap()
            .is_none());
    }

    /// `--continue` resumes the user's last conversation. Every TUI start
    /// writes an empty row and refreshes the daemon's check-in session, both
    /// stamped with the same workspace and both newer than the conversation;
    /// resolving to either loses it. A fork with no turns of its own still
    /// holds a conversation through its parent.
    #[tokio::test]
    async fn resume_lookups_skip_check_ins_and_empty_rows() {
        let db = test_db().await;
        // (id, updated_at, has_own_message, parent)
        let rows: &[(&str, i64, bool, Option<&str>)] = &[
            ("chat", 10, true, None),
            ("fork", 20, false, Some("chat")),
            ("chat-newer", 30, true, None),
            ("empty-open", 40, false, None),
            ("heartbeat:session_start", 50, true, None),
            ("heartbeat:scheduled", 60, true, None),
        ];
        for (id, updated_at, has_message, parent) in rows {
            db.upsert_session(id, 1, *updated_at, 0, "m", "")
                .await
                .unwrap();
            db.set_session_workspace(id, "/w/a", Some("/w/a"))
                .await
                .unwrap();
            if *has_message {
                db.insert_message(id, "user", Some("hi"), None, None, None, None)
                    .await
                    .unwrap();
            }
            if let Some(parent) = parent {
                db.conn
                    .execute(
                        "UPDATE sessions SET parent_session_id = ?1, forked_from_message_id = 1 \
                         WHERE id = ?2",
                        params!(*parent, *id),
                    )
                    .await
                    .unwrap();
            }
        }

        let in_workspace = db
            .most_recent_session_in_workspace(Some("/w/a"), "/w/a")
            .await
            .unwrap()
            .map(|r| r.id);
        let global = db
            .most_recent_resumable_session()
            .await
            .unwrap()
            .map(|r| r.id);
        assert_eq!(in_workspace.as_deref(), Some("chat-newer"));
        assert_eq!(global.as_deref(), Some("chat-newer"));

        db.conn
            .execute("DELETE FROM messages WHERE session_id = 'chat-newer'", ())
            .await
            .unwrap();
        let in_workspace = db
            .most_recent_session_in_workspace(Some("/w/a"), "/w/a")
            .await
            .unwrap()
            .map(|r| r.id);
        assert_eq!(
            in_workspace.as_deref(),
            Some("fork"),
            "a fork resumes through its parent's history"
        );
    }

    /// A session resumed from a new directory follows its user: the stamp is
    /// last-write-wins, and clearing the repo root (repo → plain dir) must
    /// actually clear it rather than leave a stale association.
    #[tokio::test]
    async fn workspace_stamp_is_last_write_wins() {
        let db = test_db().await;
        db.upsert_session("s", 1, 2, 0, "m", "").await.unwrap();
        db.insert_message("s", "user", Some("hi"), None, None, None, None)
            .await
            .unwrap();
        db.set_session_workspace("s", "/w/a", Some("/w/a"))
            .await
            .unwrap();
        db.set_session_workspace("s", "/elsewhere", None)
            .await
            .unwrap();

        let row = db.session_by_id("s").await.unwrap().unwrap();
        assert_eq!(row.cwd.as_deref(), Some("/elsewhere"));
        assert_eq!(
            row.git_repo_root, None,
            "re-stamp must clear a stale repo root"
        );
        assert!(
            db.most_recent_session_in_workspace(Some("/w/a"), "/w/a")
                .await
                .unwrap()
                .is_none(),
            "old workspace must no longer resolve the moved session"
        );
    }

    /// The sweep ages on last activity, not creation. A conversation started
    /// long ago but used recently is active — archiving it by birthday is the
    /// failure this ordering exists to prevent.
    #[tokio::test]
    async fn auto_archive_ages_on_activity_and_spares_pinned_sessions() {
        let db = test_db().await;
        let day = 86_400i64;
        let now = 100 * day;
        let cutoff = now - 30 * day;

        // created long ago, used long ago → archive
        db.upsert_session("stale", day, now - 60 * day, 0, "m", "Stale")
            .await
            .unwrap();
        // created long ago, used yesterday → keep
        db.upsert_session("old-but-active", day, now - day, 0, "m", "Active")
            .await
            .unwrap();
        // stale but pinned → keep
        db.upsert_session("pinned", day, now - 60 * day, 0, "m", "Pinned")
            .await
            .unwrap();
        assert!(db.set_session_pinned("pinned", true).await.unwrap());

        let archived = db.archive_stale_sessions(cutoff, now).await.unwrap();
        assert_eq!(archived, 1, "only the genuinely stale session archives");

        let listed = db.list_sessions(100).await.unwrap();
        let by_id = |id: &str| {
            listed
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("session {id} missing from listing"))
        };
        assert!(by_id("stale").archived_at.is_some());
        assert!(by_id("old-but-active").archived_at.is_none());
        assert!(by_id("pinned").archived_at.is_none());
        assert!(by_id("pinned").pinned);

        // Archiving is idempotent — a second sweep must not re-stamp or
        // re-count an already-archived session.
        assert_eq!(db.archive_stale_sessions(cutoff, now).await.unwrap(), 0);

        // And it is reversible: nothing was deleted.
        assert!(db.unarchive_session("stale").await.unwrap());
        let after = db.list_sessions(100).await.unwrap();
        assert!(after
            .iter()
            .find(|s| s.id == "stale")
            .unwrap()
            .archived_at
            .is_none());
    }

    /// An explicit archive is a user decision, so it must not inherit the
    /// sweep's guards: a pinned session still closes on request, and the stamp
    /// is reversible. Only a nonexistent id reports "nothing archived".
    #[tokio::test]
    async fn explicit_archive_overrides_pin_and_reverses() {
        let db = test_db().await;
        db.upsert_session("kept", 1, 10, 0, "m", "Kept")
            .await
            .unwrap();
        db.upsert_session("closed", 1, 20, 0, "m", "Closed")
            .await
            .unwrap();
        assert!(db.set_session_pinned("closed", true).await.unwrap());

        assert!(db.set_session_archived("closed", Some(777)).await.unwrap());
        let row = db.session_by_id("closed").await.unwrap().unwrap();
        assert_eq!(
            row.archived_at,
            Some(777),
            "an explicit close must archive even a pinned session"
        );
        assert!(row.pinned, "closing must not silently drop the pin");
        assert!(
            db.session_by_id("kept")
                .await
                .unwrap()
                .unwrap()
                .archived_at
                .is_none(),
            "archiving one session must not touch its neighbours"
        );

        // Re-archiving re-stamps rather than reporting a miss — the timestamp
        // records the latest close.
        assert!(db.set_session_archived("closed", Some(888)).await.unwrap());
        assert_eq!(
            db.session_by_id("closed")
                .await
                .unwrap()
                .unwrap()
                .archived_at,
            Some(888)
        );

        assert!(db.set_session_archived("closed", None).await.unwrap());
        assert!(db
            .session_by_id("closed")
            .await
            .unwrap()
            .unwrap()
            .archived_at
            .is_none());

        assert!(
            !db.set_session_archived("no-such-session", Some(1))
                .await
                .unwrap(),
            "a miss must be reported, not silently succeed"
        );
    }

    /// Resuming an archived conversation must un-archive it. Archiving keys on
    /// inactivity, so activity has to undo it — otherwise a session the user
    /// went back to stays hidden from every listing forever, and the sweep
    /// never revisits it to notice.
    #[tokio::test]
    async fn writing_to_an_archived_session_brings_it_back() {
        let db = test_db().await;
        let day = 86_400i64;
        let now = 100 * day;

        db.upsert_session("resumed", day, now - 60 * day, 0, "m", "Resumed")
            .await
            .unwrap();
        assert_eq!(
            db.archive_stale_sessions(now - 30 * day, now)
                .await
                .unwrap(),
            1
        );

        // The user picks it back up and sends a message.
        db.upsert_session("resumed", day, now, 12, "m", "Resumed")
            .await
            .unwrap();

        let row = db.session_by_id("resumed").await.unwrap().unwrap();
        assert!(
            row.archived_at.is_none(),
            "an archived session that is written to must return to listings"
        );
    }

    /// A pin is the per-session escape hatch from daemon-wide retention. If
    /// the purge ignored it, the only way to keep one conversation would be to
    /// keep every conversation.
    #[tokio::test]
    async fn retention_purge_spares_pinned_sessions() {
        let db = test_db().await;
        db.upsert_session("keep", 0, 0, 0, "m", "Keep")
            .await
            .unwrap();
        db.upsert_session("drop", 0, 0, 0, "m", "Drop")
            .await
            .unwrap();
        assert!(db.set_session_pinned("keep", true).await.unwrap());

        for session in ["keep", "drop"] {
            db.conn
                .execute(
                    "INSERT INTO messages (session_id, role, content, created_at) \
                     VALUES (?1, 'user', 'ancient', 100)",
                    params!(session),
                )
                .await
                .unwrap();
        }

        let purged = db.purge_messages_before(1_000).await.unwrap();
        assert_eq!(purged, 1, "only the unpinned session's message is purged");

        async fn remaining(db: &Database, session: &str) -> i64 {
            let mut rows = db
                .conn
                .query(
                    "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                    params!(session),
                )
                .await
                .unwrap();
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
        }
        assert_eq!(
            remaining(&db, "keep").await,
            1,
            "pinned session keeps its history"
        );
        assert_eq!(remaining(&db, "drop").await, 0);
    }

    /// Pinning promises the session is kept — so it must never silently fall
    /// out of the recency window and become unreachable in the picker. The
    /// catch-up must not, however, let a pin bypass the archive filter, and
    /// must not duplicate a pinned row that is already inside the window.
    #[tokio::test]
    async fn pinned_sessions_survive_the_recency_window() {
        let db = test_db().await;
        let day = 86_400i64;
        let now = 100 * day;

        // Old rows, all outside the recency window below.
        db.upsert_session("old-pinned", day, now - 90 * day, 0, "m", "Old pinned")
            .await
            .unwrap();
        db.upsert_session("old-unpinned", day, now - 89 * day, 0, "m", "Old unpinned")
            .await
            .unwrap();
        db.upsert_session("old-archived", day, now - 88 * day, 0, "m", "Old archived")
            .await
            .unwrap();
        // Archive first (the sweep skips pinned rows), then pin, so the row is
        // both archived and pinned via public API only.
        assert_eq!(
            db.archive_stale_sessions(now - 80 * day, now)
                .await
                .unwrap(),
            3
        );
        assert!(db.unarchive_session("old-pinned").await.unwrap());
        assert!(db.unarchive_session("old-unpinned").await.unwrap());
        assert!(db.set_session_pinned("old-pinned", true).await.unwrap());
        assert!(db.set_session_pinned("old-archived", true).await.unwrap());

        // Recent rows filling the window, one of them pinned.
        for i in 0..4i64 {
            db.upsert_session(&format!("recent-{i}"), now, now - i, 0, "m", "Recent")
                .await
                .unwrap();
        }
        assert!(db.set_session_pinned("recent-0", true).await.unwrap());

        let listed = db.list_sessions(4).await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();

        // The old pinned session is reachable despite being outside the
        // window — and exactly once.
        assert_eq!(
            ids.iter().filter(|id| **id == "old-pinned").count(),
            1,
            "pinned session outside the recency window must appear exactly once, got {ids:?}"
        );
        // An in-window pinned session is not duplicated by the catch-up.
        assert_eq!(ids.iter().filter(|id| **id == "recent-0").count(), 1);
        // A pin does not bypass the archive filter.
        assert!(
            !ids.contains(&"old-archived"),
            "archived pinned session must stay out of the catch-up"
        );
        // Unpinned old sessions stay outside the window.
        assert!(!ids.contains(&"old-unpinned"));
        // Order stays recency-descending overall (pinned extras are older
        // than every window row, so they land at the tail) — callers like
        // "most recent session" lookups rely on `first()` semantics.
        assert!(
            listed
                .windows(2)
                .all(|w| w[0].updated_at >= w[1].updated_at),
            "listing must stay sorted by updated_at DESC"
        );
        assert_eq!(listed.first().unwrap().id, "recent-0");
        // The pinned flag is surfaced so UIs can render it.
        assert!(listed.iter().find(|s| s.id == "old-pinned").unwrap().pinned);
    }

    #[tokio::test]
    async fn test_upsert_and_list_sessions() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 200, 500, "gpt-4", "Session One")
            .await
            .unwrap();
        db.upsert_session("s2", 150, 300, 1000, "claude", "Session Two")
            .await
            .unwrap();

        let sessions = db.list_sessions(10).await.unwrap();
        assert!(sessions.len() >= 2);
        // Most recent updated_at first
        let s2_idx = sessions.iter().position(|s| s.id == "s2").unwrap();
        let s1_idx = sessions.iter().position(|s| s.id == "s1").unwrap();
        assert!(
            s2_idx < s1_idx,
            "s2 (updated_at=300) should come before s1 (updated_at=200)"
        );
    }

    #[tokio::test]
    async fn test_upsert_session_updates_existing() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 200, 500, "gpt-4", "Original")
            .await
            .unwrap();
        db.upsert_session("s1", 100, 300, 1000, "claude", "Updated")
            .await
            .unwrap();

        let sessions = db.list_sessions(100).await.unwrap();
        let s1 = sessions.iter().find(|s| s.id == "s1").unwrap();
        assert_eq!(s1.title, "Updated");
        assert_eq!(s1.total_tokens, 1000);
        assert_eq!(s1.model, "claude");
        // Should not have duplicates
        assert_eq!(sessions.iter().filter(|s| s.id == "s1").count(), 1);
    }

    #[tokio::test]
    async fn update_session_title_overwrites_existing_row() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 200, 500, "gpt-4", "Original")
            .await
            .unwrap();
        db.update_session_title("s1", "Auto-generated phrase")
            .await
            .unwrap();
        let row = db.session_by_id("s1").await.unwrap().unwrap();
        assert_eq!(row.title, "Auto-generated phrase");
        // updated_at must have advanced past the original 200 timestamp —
        // the title refresh is a real edit and replay tooling sorts by it.
        assert!(row.updated_at > 200, "updated_at must bump on title edit");
        // Token total + model must not be clobbered by a title-only update.
        assert_eq!(row.total_tokens, 500);
        assert_eq!(row.model, "gpt-4");
    }

    #[tokio::test]
    async fn record_session_turn_titles_an_untitled_row_and_moves_it_up() {
        let db = test_db().await;
        db.upsert_session("old", 100, 100, 0, "m", "")
            .await
            .unwrap();
        db.upsert_session("new", 200, 200, 0, "m", "")
            .await
            .unwrap();

        db.record_session_turn("old", 300, "deepseek", "Open the Borg article")
            .await
            .unwrap();

        let rows = db.list_sessions(10).await.unwrap();
        assert_eq!(rows[0].id, "old", "the conversation just used lists first");
        assert_eq!(rows[0].title, "Open the Borg article");
        assert_eq!(rows[0].model, "deepseek");
    }

    #[tokio::test]
    async fn ensure_session_row_creates_once_and_never_touches_a_resumed_row() {
        let db = test_db().await;
        db.ensure_session_row("fresh", 100, "m").await.unwrap();
        let fresh = db.session_by_id("fresh").await.unwrap().unwrap();
        assert_eq!((fresh.title.as_str(), fresh.updated_at), ("", 100));

        // Reopening a named, archived conversation must not rename it, move
        // it to the top, or bring it back from the archive.
        db.upsert_session("named", 100, 200, 0, "m", "Trip plans")
            .await
            .unwrap();
        db.set_session_archived("named", Some(250)).await.unwrap();
        db.ensure_session_row("named", 900, "other").await.unwrap();
        let named = db.session_by_id("named").await.unwrap().unwrap();
        assert_eq!(named.title, "Trip plans");
        assert_eq!(named.updated_at, 200);
        assert!(named.archived_at.is_some());
    }

    /// Regression: conversations from before turns titled their row listed
    /// as a column of "Untitled" rows until a next turn that never came.
    #[tokio::test]
    async fn backfill_titles_an_old_conversation_from_its_first_user_message() {
        let db = test_db().await;
        for (id, title) in [("old", ""), ("named", "Trip plans"), ("empty", "")] {
            db.upsert_session(id, 100, 100, 0, "m", title)
                .await
                .unwrap();
        }
        db.insert_message("old", "assistant", Some("Hello."), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            "old",
            "user",
            Some("Plan a week in Lisbon\n\n<meta/>"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.insert_message("old", "user", Some("and Porto"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            "named",
            "user",
            Some("something else"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let ids = ["old", "named", "empty"].map(String::from);
        let healed = db.backfill_session_titles(&ids).await.unwrap();

        assert_eq!(healed.len(), 1, "{healed:?}");
        assert_eq!(healed["old"], "Plan a week in Lisbon");
        for (id, title) in [
            ("old", "Plan a week in Lisbon"),
            ("named", "Trip plans"),
            ("empty", ""),
        ] {
            assert_eq!(db.session_by_id(id).await.unwrap().unwrap().title, title);
        }
    }

    #[tokio::test]
    async fn record_session_turn_keeps_a_title_already_given() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "Named by the user")
            .await
            .unwrap();
        db.record_session_turn("s1", 300, "m", "First message text")
            .await
            .unwrap();
        db.upsert_session("s2", 100, 100, 0, "m", "").await.unwrap();
        db.record_session_turn("s2", 300, "m", "").await.unwrap();

        let s1 = db.session_by_id("s1").await.unwrap().unwrap();
        assert_eq!(s1.title, "Named by the user");
        assert_eq!(s1.updated_at, 300);
        assert_eq!(db.session_by_id("s2").await.unwrap().unwrap().title, "");
    }

    #[tokio::test]
    async fn update_session_title_noop_for_missing_row() {
        let db = test_db().await;
        // Must succeed silently — the title generator races session creation
        // and we don't want a spurious error if the row hasn't landed yet.
        db.update_session_title("does-not-exist", "ignored")
            .await
            .unwrap();
        assert!(db.session_by_id("does-not-exist").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_list_sessions_respects_limit() {
        let db = test_db().await;
        for i in 0..5 {
            db.upsert_session(
                &format!("s{i}"),
                100 + i,
                200 + i,
                0,
                "model",
                &format!("Session {i}"),
            )
            .await
            .unwrap();
        }
        let sessions = db.list_sessions(2).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn test_sessions_since() {
        let db = test_db().await;
        db.upsert_session("s1", 50, 100, 0, "m", "old")
            .await
            .unwrap();
        db.upsert_session("s2", 50, 200, 0, "m", "mid")
            .await
            .unwrap();
        db.upsert_session("s3", 50, 300, 0, "m", "new")
            .await
            .unwrap();

        let recent = db.sessions_since(150).await.unwrap();
        let ids: Vec<&str> = recent.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"s2"));
        assert!(ids.contains(&"s3"));
        assert!(!ids.contains(&"s1"));
    }

    #[tokio::test]
    async fn test_insert_and_load_messages() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();

        db.insert_message("s1", "user", Some("hello"), None, None, None, None)
            .await
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
        .await
        .unwrap();
        db.insert_message("s1", "tool", Some("result"), None, Some("tc1"), None, None)
            .await
            .unwrap();

        let msgs = db.load_session_messages("s1").await.unwrap();
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

    #[tokio::test]
    async fn test_delete_session_messages() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("a"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "assistant", Some("b"), None, None, None, None)
            .await
            .unwrap();

        let deleted = db.delete_session_messages("s1").await.unwrap();
        assert_eq!(deleted, 2);

        let msgs = db.load_session_messages("s1").await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_channel_session_creates_new() {
        let db = test_db().await;
        let sid1 = db
            .resolve_channel_session("telegram", "user1")
            .await
            .unwrap();
        let sid2 = db
            .resolve_channel_session("telegram", "user1")
            .await
            .unwrap();
        assert_eq!(sid1, sid2, "same channel+sender should return same session");
    }

    #[tokio::test]
    async fn test_resolve_channel_session_different_senders() {
        let db = test_db().await;
        let sid1 = db
            .resolve_channel_session("telegram", "user1")
            .await
            .unwrap();
        let sid2 = db
            .resolve_channel_session("telegram", "user2")
            .await
            .unwrap();
        assert_ne!(
            sid1, sid2,
            "different senders should get different sessions"
        );
    }

    #[tokio::test]
    async fn test_compact_session_messages() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
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
            .await
            .unwrap();
        }

        let deleted = db.compact_session_messages("s1", 3).await.unwrap();
        assert_eq!(deleted, 7);

        let remaining = db.load_session_messages("s1").await.unwrap();
        assert_eq!(remaining.len(), 3);
        // The newest 3 should remain
        assert_eq!(remaining[0].content.as_deref(), Some("msg7"));
        assert_eq!(remaining[1].content.as_deref(), Some("msg8"));
        assert_eq!(remaining[2].content.as_deref(), Some("msg9"));
    }

    #[tokio::test]
    async fn test_delete_last_assistant_turn() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        // user -> assistant -> tool -> user -> assistant -> tool
        db.insert_message("s1", "user", Some("q1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "assistant", Some("a1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "tool", Some("t1"), None, Some("tc1"), None, None)
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("q2"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "assistant", Some("a2"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "tool", Some("t2"), None, Some("tc2"), None, None)
            .await
            .unwrap();

        let deleted = db.delete_last_assistant_turn("s1").await.unwrap();
        assert_eq!(deleted, 2); // assistant + tool

        let msgs = db.load_session_messages("s1").await.unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3].role, "user");
        assert_eq!(msgs[3].content.as_deref(), Some("q2"));
    }

    #[tokio::test]
    async fn test_delete_last_assistant_turn_only_user_messages() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("q1"), None, None, None, None)
            .await
            .unwrap();

        let deleted = db.delete_last_assistant_turn("s1").await.unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn fork_inherits_history_up_to_anchor_by_reference() {
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
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
                .await
                .unwrap(),
            );
        }
        let anchor = ids[3];

        let inherited = db
            .fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .await
            .unwrap();
        assert_eq!(inherited, 4, "child inherits the parent's first four turns");

        // Reference, not copy: the child stores NO message rows of its own.
        assert_eq!(db.load_session_messages("child").await.unwrap().len(), 0);
        assert_eq!(db.load_session_messages("parent").await.unwrap().len(), 6);

        // The child's reconstructable history is the parent's prefix, with the
        // original absolute seqs preserved (not remapped by a copy).
        let lineage = db.load_session_lineage_rows("child").await.unwrap();
        let contents: Vec<_> = lineage.iter().map(|m| m.content.clone().unwrap()).collect();
        assert_eq!(contents, vec!["m0", "m1", "m2", "m3"]);
        assert_eq!(lineage.iter().map(|r| r.id).collect::<Vec<_>>(), ids[..4]);
    }

    #[tokio::test]
    async fn fork_writes_independently() {
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
            .unwrap();
        let anchor = db
            .insert_message("parent", "user", Some("q1"), None, None, None, None)
            .await
            .unwrap();

        db.fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .await
            .unwrap();
        db.insert_message("child", "user", Some("child-only"), None, None, None, None)
            .await
            .unwrap();

        // The parent is untouched; the child owns only its post-fork row, but
        // reconstructs the inherited turn ++ its own.
        assert_eq!(db.load_session_messages("parent").await.unwrap().len(), 1);
        assert_eq!(db.load_session_messages("child").await.unwrap().len(), 1);
        let child_history = db.load_session_lineage_rows("child").await.unwrap();
        let contents: Vec<_> = child_history
            .iter()
            .map(|m| m.content.clone().unwrap())
            .collect();
        assert_eq!(contents, vec!["q1", "child-only"]);
    }

    #[tokio::test]
    async fn fork_of_fork_stitches_full_lineage() {
        let db = test_db().await;
        db.upsert_session("root", 100, 100, 0, "m", "Root")
            .await
            .unwrap();
        let r1 = db
            .insert_message("root", "user", Some("r1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("root", "user", Some("r2"), None, None, None, None)
            .await
            .unwrap();

        // Fork the root at r1, append to the child, then fork the child.
        db.fork_session("root", r1, "child", "Child", 200)
            .await
            .unwrap();
        let c1 = db
            .insert_message("child", "user", Some("c1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("child", "user", Some("c2"), None, None, None, None)
            .await
            .unwrap();
        db.fork_session("child", c1, "grandchild", "Grandchild", 300)
            .await
            .unwrap();
        db.insert_message("grandchild", "user", Some("g1"), None, None, None, None)
            .await
            .unwrap();

        // grandchild sees: root up to r1 (r1 only, not r2), child up to c1 (c1
        // only, not c2), then its own g1 — all in seq order.
        let contents: Vec<_> = db
            .load_session_lineage_rows("grandchild")
            .await
            .unwrap()
            .iter()
            .map(|m| m.content.clone().unwrap())
            .collect();
        assert_eq!(contents, vec!["r1", "c1", "g1"]);
    }

    #[tokio::test]
    async fn fork_lineage_persisted_on_session_row() {
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
            .unwrap();
        let anchor = db
            .insert_message("parent", "user", Some("q1"), None, None, None, None)
            .await
            .unwrap();
        db.fork_session("parent", anchor, "child", "Parent (fork)", 200)
            .await
            .unwrap();

        let row = db
            .session_by_id("child")
            .await
            .unwrap()
            .expect("child exists");
        assert_eq!(row.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(row.forked_from_message_id, Some(anchor));
        let parent_row = db.session_by_id("parent").await.unwrap().unwrap();
        assert_eq!(parent_row.parent_session_id, None);
        assert_eq!(parent_row.forked_from_message_id, None);
    }

    #[tokio::test]
    async fn fork_invalid_anchor_errors_and_does_not_create_session() {
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
            .unwrap();
        db.insert_message("parent", "user", Some("q1"), None, None, None, None)
            .await
            .unwrap();

        let bad = db
            .fork_session("parent", 999_999, "child", "fork", 200)
            .await;
        assert!(bad.is_err());
        assert!(db.session_by_id("child").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fork_unknown_parent_errors() {
        let db = test_db().await;
        let r = db.fork_session("ghost", 1, "child", "fork", 200).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn fork_lineage_chain_verifies_and_detects_tampering() {
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
            .unwrap();
        let a = db
            .insert_message("parent", "user", Some("p1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("parent", "assistant", Some("p2"), None, None, None, None)
            .await
            .unwrap();
        // Fork by reference at the first parent turn, then chain new child rows.
        db.fork_session("parent", a, "child", "Child", 200)
            .await
            .unwrap();
        db.insert_message("child", "user", Some("c1"), None, None, None, None)
            .await
            .unwrap();

        // The stitched lineage (parent prefix + child's own chain) verifies:
        // the session boundary legitimately restarts the chain at "0".
        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        let mut rows = db.load_session_lineage_rows("child").await.unwrap();
        assert!(crate::conversation::verify_session_chain(&key, &rows));

        // Tampering with an inherited row's content breaks verification.
        rows[0].content = Some("tampered".to_string());
        assert!(!crate::conversation::verify_session_chain(&key, &rows));
    }

    #[tokio::test]
    async fn compaction_checkpoint_survives_fork() {
        // The payoff of keying both compaction and forking off absolute,
        // immutable seqs: a compaction event in the parent is inherited by a
        // fork and still folds correctly, because the seqs it references aren't
        // remapped (no rows are copied).
        let db = test_db().await;
        db.upsert_session("parent", 100, 100, 0, "m", "Parent")
            .await
            .unwrap();
        let s0 = db
            .insert_message("parent", "user", Some("old q"), None, None, None, None)
            .await
            .unwrap();
        let s1 = db
            .insert_message("parent", "assistant", Some("old a"), None, None, None, None)
            .await
            .unwrap();
        let s2 = db
            .insert_message("parent", "user", Some("recent q"), None, None, None, None)
            .await
            .unwrap();
        // Summarize the first two turns; the checkpoint row gets its own seq.
        let cp = db
            .append_compaction_checkpoint("parent", s0, s1, "early summary", "m")
            .await
            .unwrap();

        // Fork at the checkpoint so the child inherits it, then continue.
        db.fork_session("parent", cp, "child", "Child", 200)
            .await
            .unwrap();
        db.insert_message("child", "user", Some("child q"), None, None, None, None)
            .await
            .unwrap();

        let rows = db.load_session_lineage_rows("child").await.unwrap();
        let history =
            crate::conversation::project_history(&crate::conversation::events_from_rows(rows));
        // Summary stands in for s0+s1; s2 and the child's own turn replay verbatim.
        let texts: Vec<_> = history
            .iter()
            .map(|m| m.text_content().unwrap_or("").to_string())
            .collect();
        assert_eq!(texts.len(), 3, "summary + recent q + child q");
        assert!(texts[0].contains("early summary"));
        assert_eq!(texts[1], "recent q");
        assert_eq!(texts[2], "child q");
        // Sanity: the raw inherited rows are all still present (non-destructive).
        assert!(s2 > s1 && cp > s2);
    }

    #[tokio::test]
    async fn live_compaction_range_folds_through_the_log() {
        // End-to-end at the log level: derive the checkpoint range from the
        // working history exactly as `Agent::run_llm_compaction` does, append a
        // checkpoint over it, then reconstruct from the log. A fresh worker must
        // see [protected head] ++ [summary] ++ [recent tail] — i.e. compaction
        // "falls out" of the log, with the raw rows left intact on disk.
        let db = test_db().await;
        db.upsert_session("s", 1, 1, 0, "m", "S").await.unwrap();
        for i in 0..12 {
            db.insert_message(
                "s",
                "user",
                Some(&format!("question {i} with some padding text here")),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            db.insert_message(
                "s",
                "assistant",
                Some(&format!("answer {i} with some padding text here")),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }

        // Reconstruct the working history; each message carries its log seq.
        let rows = db.load_session_lineage_rows("s").await.unwrap();
        let history =
            crate::conversation::project_history(&crate::conversation::events_from_rows(rows));

        let max_tokens = 200;
        let protect_first_n = 3;
        let plan = crate::conversation::plan_compaction_v2(&history, max_tokens, protect_first_n)
            .expect("over-budget history must compact");
        let (from_seq, to_seq) =
            crate::conversation::compaction_checkpoint_range(&history, max_tokens, protect_first_n)
                .expect("persisted middle must yield a durable range");

        db.append_compaction_checkpoint("s", from_seq, to_seq, "the summary", "m")
            .await
            .unwrap();

        let rows = db.load_session_lineage_rows("s").await.unwrap();
        let projected =
            crate::conversation::project_history(&crate::conversation::events_from_rows(rows));

        // Protected head survives verbatim.
        for i in 0..plan.head_end {
            assert_eq!(
                projected[i].text_content(),
                history[i].text_content(),
                "protected head message {i} must survive the fold"
            );
        }
        // The summary stands in for the middle, exactly once, right after the head.
        assert!(projected[plan.head_end]
            .text_content()
            .unwrap()
            .contains("the summary"));
        // The most recent turn survives.
        assert_eq!(
            projected.last().unwrap().text_content(),
            history.last().unwrap().text_content()
        );
        // Net effect: the projection is shorter than the raw history.
        assert!(projected.len() < history.len());
    }

    #[tokio::test]
    async fn lease_claim_grants_exclusive_ownership() {
        let db = test_db().await;
        db.upsert_session("s", 1, 1, 0, "m", "S").await.unwrap();

        // Worker A claims the free lease.
        let a = db
            .claim_session_lease("s", "worker-a", 1000, 60)
            .await
            .unwrap()
            .expect("free lease is grantable");
        assert_eq!(a.owner, "worker-a");
        assert!(a.epoch >= 1, "takeover from the default epoch advances it");

        // Worker B cannot claim while A's lease is live.
        assert!(db
            .claim_session_lease("s", "worker-b", 1010, 60)
            .await
            .unwrap()
            .is_none());

        // A re-claiming (re-entrant) keeps the same epoch so its handle stays valid.
        let a2 = db
            .claim_session_lease("s", "worker-a", 1020, 60)
            .await
            .unwrap()
            .expect("re-entrant claim succeeds");
        assert_eq!(a2.epoch, a.epoch);
    }

    #[tokio::test]
    async fn lease_expiry_allows_takeover_and_fences_dead_worker() {
        let db = test_db().await;
        db.upsert_session("s", 1, 1, 0, "m", "S").await.unwrap();
        let a = db
            .claim_session_lease("s", "worker-a", 1000, 60)
            .await
            .unwrap()
            .unwrap();

        // After the deadline, B takes over — epoch advances past A's.
        let b = db
            .claim_session_lease("s", "worker-b", 2000, 60)
            .await
            .unwrap()
            .expect("expired lease is reclaimable");
        assert!(b.epoch > a.epoch, "takeover fences the previous epoch");

        // A revives and tries to renew/release with its stale epoch — both no-op.
        assert!(!db
            .renew_session_lease("s", "worker-a", a.epoch, 2010, 60)
            .await
            .unwrap());
        assert!(!db
            .release_session_lease("s", "worker-a", a.epoch)
            .await
            .unwrap());

        // B still holds it (A's fenced release didn't clear it).
        assert!(db
            .claim_session_lease("s", "worker-c", 2020, 60)
            .await
            .unwrap()
            .is_none());

        // B renews and releases successfully.
        assert!(db
            .renew_session_lease("s", "worker-b", b.epoch, 2020, 60)
            .await
            .unwrap());
        assert!(db
            .release_session_lease("s", "worker-b", b.epoch)
            .await
            .unwrap());
        // Released → claimable again.
        assert!(db
            .claim_session_lease("s", "worker-c", 2030, 60)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn expire_stale_session_leases_reclaims_only_the_dead() {
        let db = test_db().await;
        db.upsert_session("dead", 1, 1, 0, "m", "D").await.unwrap();
        db.upsert_session("live", 1, 1, 0, "m", "L").await.unwrap();
        db.claim_session_lease("dead", "w1", 1000, 60)
            .await
            .unwrap()
            .unwrap();
        db.claim_session_lease("live", "w2", 1000, 60)
            .await
            .unwrap()
            .unwrap();

        // At t=1100, "dead" (expiry 1060) is stale; "live" was renewed to 2000.
        db.renew_session_lease("live", "w2", 1, 1100, 900)
            .await
            .unwrap();
        let reclaimed = db.expire_stale_session_leases(1100).await.unwrap();
        assert_eq!(reclaimed, 1, "only the dead lease is freed");

        // "dead" is now claimable; "live" is still held.
        assert!(db
            .claim_session_lease("dead", "w3", 1100, 60)
            .await
            .unwrap()
            .is_some());
        assert!(db
            .claim_session_lease("live", "w3", 1100, 60)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn reclaim_all_session_leases_frees_even_unexpired_leases() {
        // Boot regression: a crash-restart within the lease TTL gives the new
        // daemon a different (pid-based) worker id, so it can't reclaim its own
        // still-unexpired leases re-entrantly. Boot must free them all, or the
        // session is locked until the TTL lapses.
        let db = test_db().await;
        db.upsert_session("s", 1, 1, 0, "m", "S").await.unwrap();
        // Held by the previous daemon with a deadline far in the future.
        db.claim_session_lease("s", "borgd-111", 1000, 3600)
            .await
            .unwrap()
            .unwrap();
        // The TTL-based reclaim leaves the unexpired lease alone...
        assert_eq!(db.expire_stale_session_leases(1100).await.unwrap(), 0);
        // ...so a fresh-pid worker would be locked out without the boot reclaim.
        assert!(db
            .claim_session_lease("s", "borgd-222", 1100, 3600)
            .await
            .unwrap()
            .is_none());

        // Boot reclaim frees it unconditionally; the new worker then claims it.
        assert_eq!(db.reclaim_all_session_leases().await.unwrap(), 1);
        let lease = db
            .claim_session_lease("s", "borgd-222", 1100, 3600)
            .await
            .unwrap()
            .expect("freed lease must be claimable after boot reclaim");
        // Epoch advanced past the previous holder's, fencing any zombie writer.
        assert!(lease.epoch >= 2);
    }

    #[tokio::test]
    async fn test_log_channel_message() {
        let db = test_db().await;
        let rowid = db
            .log_channel_message("telegram", "user1", "inbound", Some("hello"), None, None)
            .await
            .unwrap();
        assert!(rowid > 0);
    }

    #[tokio::test]
    async fn test_update_channel_session_id() {
        let db = test_db().await;
        let _sid = db
            .resolve_channel_session("telegram", "user1")
            .await
            .unwrap();

        let updated = db
            .update_channel_session_id("telegram", "user1", "new-session-id")
            .await
            .unwrap();
        assert!(updated);

        // Non-existent returns false
        let not_found = db
            .update_channel_session_id("telegram", "nobody", "x")
            .await
            .unwrap();
        assert!(!not_found);
    }

    #[tokio::test]
    async fn appended_messages_form_a_valid_hmac_chain() {
        // Every message written through insert_message must extend a verifiable
        // per-session chain — that's the tamper-evidence the log relies on.
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("hello"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "assistant", Some("hi there"), None, None, None, None)
            .await
            .unwrap();

        let rows = db.load_session_messages("s1").await.unwrap();
        assert!(rows.iter().all(|r| r.hmac.is_some()));
        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        assert!(crate::conversation::verify_session_chain(&key, &rows));
    }

    #[tokio::test]
    async fn batched_append_chain_is_identical_to_sequential_appends() {
        // The batched flush must be a pure transaction-boundary optimization:
        // the HMAC chain it produces has to be byte-identical to the same
        // events appended one at a time, or chain verification would depend
        // on which code path wrote the rows.
        let db = test_db().await;
        db.upsert_session("seq", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.upsert_session("bat", 100, 100, 0, "m", "test")
            .await
            .unwrap();

        // Same timestamp for every row so created_at (an HMAC input) cannot
        // differ between the two paths across a second boundary.
        let ts = "2026-08-04T12:00:00Z";
        let events: Vec<SessionEventDraft> = vec![
            SessionEventDraft {
                role: "assistant".into(),
                content: Some("running tools".into()),
                tool_calls_json: Some(r#"[{"id":"c1"},{"id":"c2"}]"#.into()),
                timestamp: Some(ts.into()),
                ..Default::default()
            },
            SessionEventDraft {
                role: "tool".into(),
                content: Some("result one".into()),
                tool_call_id: Some("c1".into()),
                timestamp: Some(ts.into()),
                ..Default::default()
            },
            SessionEventDraft {
                role: "tool".into(),
                content: Some("result two".into()),
                tool_call_id: Some("c2".into()),
                timestamp: Some(ts.into()),
                ..Default::default()
            },
        ];

        for e in &events {
            db.append_session_event(
                "seq",
                &e.role,
                e.event_kind.as_deref(),
                e.content.as_deref(),
                e.tool_calls_json.as_deref(),
                e.tool_call_id.as_deref(),
                e.timestamp.as_deref(),
                e.content_parts_json.as_deref(),
                e.event_json.as_deref(),
            )
            .await
            .unwrap();
        }
        let ids = db.append_session_events("bat", &events).await.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "ids ascend in order");

        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        let seq_rows = db.load_session_messages("seq").await.unwrap();
        let bat_rows = db.load_session_messages("bat").await.unwrap();
        assert!(crate::conversation::verify_session_chain(&key, &seq_rows));
        assert!(crate::conversation::verify_session_chain(&key, &bat_rows));

        // Chains only differ through the session id (an HMAC input); link
        // structure must match: same prev-linking, same row payloads.
        assert_eq!(seq_rows.len(), bat_rows.len());
        for (s, b) in seq_rows.iter().zip(&bat_rows) {
            assert_eq!(s.role, b.role);
            assert_eq!(s.content, b.content);
            assert_eq!(s.tool_call_id, b.tool_call_id);
        }
        assert_eq!(
            bat_rows[0].prev_hmac.as_deref(),
            Some("0"),
            "batch chain starts from the genesis link"
        );
        assert_eq!(
            bat_rows[1].prev_hmac, bat_rows[0].hmac,
            "second batched row links to the first"
        );
        assert_eq!(
            bat_rows[2].prev_hmac, bat_rows[1].hmac,
            "third batched row links to the second"
        );
    }

    #[tokio::test]
    async fn tampering_message_content_breaks_the_chain() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("transfer $10"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message("s1", "assistant", Some("ok"), None, None, None, None)
            .await
            .unwrap();

        // Tamper the persisted content out from under the HMAC.
        db.conn
            .execute(
                "UPDATE messages SET content = 'transfer $1000' WHERE session_id = 's1' AND role = 'user'",
                (),
            )
            .await
            .unwrap();

        let rows = db.load_session_messages("s1").await.unwrap();
        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        assert!(
            !crate::conversation::verify_session_chain(&key, &rows),
            "edited content must fail chain verification"
        );
    }

    #[tokio::test]
    async fn unchained_row_fails_chain_verification() {
        // Every persist path chains, so a NULL-hmac row means someone wrote to
        // the DB directly. That is the tampering the chain exists to catch —
        // it must not be waved through just because there is nothing to check.
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        // Smuggle in a row behind the chained writer's back.
        db.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at) VALUES ('s1','user','smuggled',100)",
                (),
            )
            .await
            .unwrap();
        // Append through the chained path.
        db.insert_message("s1", "assistant", Some("reply"), None, None, None, None)
            .await
            .unwrap();

        let rows = db.load_session_messages("s1").await.unwrap();
        assert!(rows[0].hmac.is_none(), "smuggled row is unchained");
        assert!(rows[1].hmac.is_some(), "appended row is chained");
        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        assert!(
            !crate::conversation::verify_session_chain(&key, &rows),
            "an unchained row must fail verification"
        );
    }

    #[tokio::test]
    async fn resume_after_crash_mid_tool_call_yields_valid_history() {
        // The log-is-the-agent reliability property: the assistant message
        // (carrying its tool call) is persisted BEFORE the tool dispatches, so
        // a crash before the result lands still leaves a recoverable log.
        // Reconstructing from the log and normalizing must repair the dangling
        // call — no tool_call is left without a tool_result.
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        db.insert_message("s1", "user", Some("do it"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            "s1",
            "assistant",
            Some("working on it"),
            Some(r#"[{"id":"c1","type":"function","function":{"name":"run_shell","arguments":"{}"}}]"#),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // Crash here: the tool result for c1 is never persisted.

        // Reconstruct from the durable log, exactly as a fresh executor would.
        let rows = db.load_session_messages("s1").await.unwrap();
        let events = crate::conversation::events_from_rows(rows);
        let mut history = crate::conversation::project_history(&events);
        crate::conversation::normalize_history(&mut history);

        let call_ids: std::collections::HashSet<String> = history
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flatten()
            .map(|tc| tc.id.clone())
            .collect();
        let result_ids: std::collections::HashSet<String> = history
            .iter()
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        assert!(
            call_ids.is_subset(&result_ids),
            "every tool_call must have a tool_result after resume+normalize"
        );
        assert!(
            history
                .iter()
                .any(|m| m.text_content().unwrap_or("").contains("aborted")),
            "the unresolved call must be repaired with an aborted-result marker"
        );
    }

    #[tokio::test]
    async fn test_count_session_messages() {
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        for _ in 0..5 {
            db.insert_message("s1", "user", Some("msg"), None, None, None, None)
                .await
                .unwrap();
        }
        assert_eq!(db.count_session_messages("s1").await.unwrap(), 5);
    }
}
