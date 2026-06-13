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

impl Database {
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
                updated_at = ?3, total_tokens = ?4, model = ?5, title = ?6",
                params!(id, created_at, updated_at, total_tokens, model, title),
            )
            .await?;
        Ok(())
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

    /// Fetch a single session row by exact id. Returns `None` if absent.
    pub async fn session_by_id(&self, id: &str) -> Result<Option<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
             FROM sessions WHERE id = ?1",
                params!(id),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(SessionRow {
                id: row.get::<String>(0)?,
                created_at: row.get::<i64>(1)?,
                updated_at: row.get::<i64>(2)?,
                total_tokens: row.get::<i64>(3)?,
                model: row.get::<String>(4)?,
                title: row.get::<String>(5)?,
                parent_session_id: row.get::<Option<String>>(6)?,
                forked_from_message_id: row.get::<Option<i64>>(7)?,
            })),
            None => Ok(None),
        }
    }

    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
                params!(limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SessionRow {
                id: row.get::<String>(0)?,
                created_at: row.get::<i64>(1)?,
                updated_at: row.get::<i64>(2)?,
                total_tokens: row.get::<i64>(3)?,
                model: row.get::<String>(4)?,
                title: row.get::<String>(5)?,
                parent_session_id: row.get::<Option<String>>(6)?,
                forked_from_message_id: row.get::<Option<i64>>(7)?,
            });
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
                "SELECT id, created_at, updated_at, total_tokens, model, title,
                    parent_session_id, forked_from_message_id
             FROM sessions WHERE updated_at >= ?1 ORDER BY updated_at DESC
             LIMIT ?2",
                params!(since, SESSIONS_SINCE_HARD_CAP),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SessionRow {
                id: row.get::<String>(0)?,
                created_at: row.get::<i64>(1)?,
                updated_at: row.get::<i64>(2)?,
                total_tokens: row.get::<i64>(3)?,
                model: row.get::<String>(4)?,
                title: row.get::<String>(5)?,
                parent_session_id: row.get::<Option<String>>(6)?,
                forked_from_message_id: row.get::<Option<i64>>(7)?,
            });
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

    // ── Message persistence ──

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
    /// This is the single durable-append primitive: conversational messages
    /// (`event_kind = None`) and fine-grained lifecycle events
    /// (`event_kind = Some(tag)`, payload in `event_json`) both flow through
    /// here. Mirrors `record_vitals_event`: `BEGIN IMMEDIATE` serializes the
    /// read-prev-then-write so two appenders can't fork the chain. Returns the
    /// new row's `id` (the event's `seq`).
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
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: Result<i64> = async {
            let now = chrono::Utc::now().timestamp();
            // Per-session chain: link from the last event in THIS session, so
            // each session's log is its own chain and a fork starts fresh.
            // Last chained hmac in THIS session, or "0" for the first event —
            // whether there's no prior row at all or the prior row is an
            // unchained legacy row (NULL hmac).
            let prev_hmac: String = {
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
            let hmac = crate::conversation::compute_session_event_hmac(
                &self.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN),
                &prev_hmac,
                &crate::conversation::SessionEventHashInput {
                    session_id,
                    role,
                    event_kind,
                    content,
                    tool_calls_json,
                    tool_call_id,
                    timestamp,
                    content_parts_json,
                    event_json,
                    created_at: now,
                },
            );
            self.conn.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls_json, tool_call_id,
                    timestamp, created_at, content_parts_json, event_kind, event_json, hmac, prev_hmac)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params!(
                    session_id, role, content, tool_calls_json, tool_call_id, timestamp, now,
                    content_parts_json, event_kind, event_json, hmac, prev_hmac
                ),
            ).await?;
            Ok(self.conn.last_insert_rowid())
        }
        .await;

        match result {
            Ok(id) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(id)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("sessions: rollback after append_session_event failed: {re}");
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

        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(v)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("sessions: rollback after claim_session_lease failed: {re}");
                }
                Err(e)
            }
        }
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

    // ── Channel sessions ──

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
    async fn legacy_unchained_prefix_then_chained_suffix_verifies() {
        // Sessions that existed before chaining have NULL-hmac rows. A chained
        // row appended after upgrade must still verify against a "0" baseline.
        let db = test_db().await;
        db.upsert_session("s1", 100, 100, 0, "m", "test")
            .await
            .unwrap();
        // Simulate a legacy row: direct insert with NULL hmac/prev_hmac.
        db.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at) VALUES ('s1','user','legacy',100)",
                (),
            )
            .await
            .unwrap();
        // Now append through the chained path.
        db.insert_message(
            "s1",
            "assistant",
            Some("modern reply"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let rows = db.load_session_messages("s1").await.unwrap();
        assert!(rows[0].hmac.is_none(), "legacy row stays unchained");
        assert!(rows[1].hmac.is_some(), "appended row is chained");
        let key = db.derive_hmac_key(crate::conversation::SESSION_HMAC_DOMAIN);
        assert!(crate::conversation::verify_session_chain(&key, &rows));
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
