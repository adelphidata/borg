//! Session and message-history lifecycle for `Agent`.
//!
//! Keeps session boundaries
//! (`load_session` / `new_session` / `end_session`), message persistence
//! (`persist_message`, `log_and_persist`, `append_assistant_message`,
//! `skip_tool_call`), and the secret-redaction helpers that wrap tool
//! output (`maybe_redact`, `truncate_and_redact`) in one focused module.
//!
//! All entry points are methods on `Agent`; the trait-like surface is
//! preserved via an additional `impl Agent` block — Rust permits multiple
//! impls of the same type within a crate.

use anyhow::Result;
use tracing::warn;

use crate::hooks::{HookData, HookPoint};
use crate::logging::log_message;
use crate::security::secrets::redact_secrets;
use crate::session::Session;
use crate::types::Message;
use crate::util::truncate::truncate_output;

use super::tool_validation::has_unresolved_tool_calls;
use super::Agent;

impl Agent {
    /// Restore a session's history into this agent.
    pub fn load_session(&mut self, id: &str) -> Result<()> {
        let session = Session::load(id)?;
        self.history = session.messages.clone();
        self.session = session;
        // Goal tracking belongs to the session it was armed in; carrying it
        // over would read the new session's missing goal row as a user clear.
        self.goal_runtime.forget();
        Ok(())
    }

    /// Start a new session, clearing history.
    pub async fn new_session(&mut self) {
        // Fire SessionEnd for the previous session if any turns were executed
        if self.turn_count > 0 {
            let hook_ctx = self.hook_ctx(
                HookPoint::SessionEnd,
                HookData::SessionEnd {
                    session_id: self.session.meta.id.clone(),
                    total_turns: self.turn_count,
                },
            );
            self.hook_registry.dispatch(&hook_ctx).await;
            self.flush_short_term_memory().await;
        }

        self.history.clear();
        self.session = Session::new();
        self.short_term_memory = crate::memory::short_term_memory::ShortTermMemory::new(
            self.session.meta.id.clone(),
            2000,
        );
        // A fresh session gets its own one-shot auto-title pass.
        self.auto_title_spawned = false;
        // Don't carry a prior session's failover identity into a new session —
        // its first turn runs on the configured primary until proven otherwise.
        self.active_failover_identity = None;
        // Goal tracking belongs to the prior session; carrying it over would
        // read the new session's missing goal row as a user clear.
        self.goal_runtime.forget();
        // Drop the system-prompt input caches (identity, resolved credentials,
        // project docs, skills). Otherwise a long-lived `Agent` reused across a
        // reset would assemble the new session's prompt from the prior session's
        // cached IDENTITY.md / .env values — a stale-context leak. They lazily
        // reload on the next turn.
        self.caches = super::system_prompt_cache::SystemPromptCache::default();

        let hook_ctx = self.hook_ctx(
            HookPoint::SessionStart,
            HookData::SessionStart {
                session_id: self.session.meta.id.clone(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx).await;
    }

    /// Flush accumulated short-term memory facts to the daily log entry in
    /// the database. Called on session end so facts the agent collected during
    /// the session are promoted into long-term memory (via nightly
    /// consolidation).
    ///
    /// Reuses the agent's open DB handle when available so a session-end
    /// flush doesn't race a fresh `Database::open()` against the agent's own
    /// writes (and, in tests, so the flush hits the same in-memory DB the
    /// caller is inspecting).
    ///
    /// Fire-and-forget: background shutdown must not crash on a storage error.
    /// Any failure is logged but otherwise ignored.
    pub async fn flush_short_term_memory(&self) {
        let facts = self.short_term_memory.facts_as_text();
        if facts.trim().is_empty() {
            return;
        }
        let guard = self.db_guard().await;
        if let Err(e) = crate::memory::consolidation::flush_short_term_to_daily_with_optional_db(
            guard.as_ref(),
            &facts,
        )
        .await
        {
            warn!("flush_short_term_memory failed: {e}");
        }
    }

    /// Signal that this agent's owning context is ending (e.g. CLI shutdown,
    /// gateway handler finishing). Fires SessionEnd and flushes short-term
    /// memory even when no `new_session()` call follows. Safe to call multiple
    /// times; a no-op if no turns were executed.
    pub async fn end_session(&mut self) {
        if self.turn_count == 0 {
            return;
        }
        let hook_ctx = self.hook_ctx(
            HookPoint::SessionEnd,
            HookData::SessionEnd {
                session_id: self.session.meta.id.clone(),
                total_turns: self.turn_count,
            },
        );
        self.hook_registry.dispatch(&hook_ctx).await;
        self.flush_short_term_memory().await;
        // Clear turn_count so a repeat end_session() is a no-op.
        self.turn_count = 0;
    }

    /// Auto-save current session state.
    ///
    /// A transient disk I/O error (EIO) is retried a couple of times before
    /// it is allowed to classify as a fatal storage fault — one hiccup must
    /// not abort the turn. Durable exhaustion (ENOSPC/EDQUOT/EROFS) fails
    /// immediately as before.
    pub fn auto_save(&mut self) {
        self.session.update_from_history(&self.history);
        if let Err(e) =
            crate::db::with_transient_io_retry_sync("session auto-save", || self.session.save())
        {
            warn!("Failed to auto-save session: {e}");
            if crate::db::is_storage_exhausted(&e) {
                self.storage_fault
                    .get_or_insert_with(|| storage_fault_message(&e));
            }
        }
    }

    /// Mirror a finished turn onto the session's DB row.
    ///
    /// Session listings read the row, not the JSON file `auto_save` writes,
    /// so without this every conversation lists untitled and in the order it
    /// was opened. Runs for every way a turn ends — a cancelled or cut-short
    /// turn still happened. Best-effort: a listing detail must not fail the
    /// turn.
    pub(super) async fn record_turn_on_session_row(&self) {
        let meta = &self.session.meta;
        // A turn that ended before `auto_save` has not named the session yet.
        let title = if meta.title == crate::session::DEFAULT_TITLE {
            crate::session::title_from_history(&self.history).unwrap_or_default()
        } else {
            meta.title.clone()
        };
        let now = chrono::Utc::now().timestamp();
        let guard = self.db_guard().await;
        let Some(ref db) = *guard else { return };
        if let Err(e) = db
            .record_session_turn(&meta.id, now, &self.config.llm.model, &title)
            .await
        {
            warn!(session_id = %meta.id, "recording the turn on the session row failed: {e}");
        }
    }

    /// Append a system-injected assistant message to the agent's history.
    ///
    /// Used to mirror text the user already saw on this client (e.g. a
    /// heartbeat that was streamed in by the daemon while the TUI was open)
    /// into the active session so the next user reply has it in context.
    /// Without this, the user's "yes please" reply lands in a session whose
    /// history has no record of the heartbeat, and the agent answers the
    /// previous turn instead.
    ///
    /// Refuses to inject if doing so would split an `assistant(tool_calls)`
    /// message from its outstanding `tool` results — both Anthropic and
    /// OpenAI reject orphaned tool_use blocks on the next turn, so a
    /// well-meaning mid-turn mirror would brick the session.
    pub async fn append_assistant_message(&mut self, text: &str) {
        if has_unresolved_tool_calls(&self.history) {
            warn!(
                "append_assistant_message: refused — last assistant turn has unresolved \
                 tool_calls; injecting now would orphan them. Dropping the mirrored message."
            );
            return;
        }
        self.persist_message(Message::assistant(text)).await;
    }

    /// Log a message and push it to history + SQLite in one step.
    pub(super) async fn log_and_persist(&mut self, msg: Message) {
        log_message(&msg);
        self.persist_message(msg).await;
    }

    /// Log a batch of messages and push them to history + SQLite as one
    /// transaction (see [`Self::persist_messages`]).
    pub(super) async fn log_and_persist_batch(&mut self, msgs: Vec<Message>) {
        for msg in &msgs {
            log_message(msg);
        }
        self.persist_messages(msgs).await;
    }

    /// Record a skipped/cancelled tool call as a tool_result message.
    pub(super) async fn skip_tool_call(&mut self, tool_call_id: &str, reason: &str) {
        self.log_and_persist(Message::tool_result(tool_call_id, reason))
            .await;
    }

    /// Optionally redact secrets from a string based on config.
    pub(super) fn maybe_redact(&self, s: String) -> String {
        if self.config.security.secret_detection {
            redact_secrets(&s)
        } else {
            s
        }
    }

    /// Truncate and optionally redact secrets from raw tool output, spilling
    /// the full text to disk when it does not fit.
    ///
    /// The configured `tool_output_max_tokens` (default 10k) is a fixed ceiling
    /// that ignores the model — on a small-context model a single 10k-token
    /// tool result is larger than the entire window and crowds out history. We
    /// clamp the per-result cap to a quarter of the active context window so
    /// one result can never dominate, while large-context models keep the
    /// configured ceiling unchanged.
    ///
    /// Truncation alone destroys the omitted middle: the model's only route
    /// back to it is re-running the tool, which costs another call and is not
    /// safe for a tool that is not idempotent. `run_shell` already spills its
    /// own streams; this covers every *other* tool, MCP results above all,
    /// where a server returning an un-paginated payload otherwise loses
    /// everything past the cap with no way to recover it.
    ///
    /// Redaction runs before truncation now, not after, so the file on disk
    /// gets the same secret hygiene as the text handed to the model. The cost
    /// is that the regex pass scans the whole result rather than the ~40 KB
    /// that survived truncation. That is accepted deliberately: the spill is
    /// bounded by `SPILL_KEEP_MAX_BYTES`, and writing an unredacted secret to
    /// a file that lives for 24h is the worse trade.
    pub(super) fn truncate_and_redact(&self, raw: &str) -> String {
        self.truncate_redact_and_spill(raw, spill_dir().as_deref())
    }

    /// [`Self::truncate_and_redact`] with the spill directory supplied rather
    /// than resolved from `BORG_HOME`.
    ///
    /// The directory is the one input that would otherwise force a test to
    /// mutate process-global env, which races every other test in the binary
    /// that *reads* `BORG_HOME`. Passing it in keeps the cap, redaction and
    /// spill wiring testable against a tempdir with no env involved.
    pub(super) fn truncate_redact_and_spill(
        &self,
        raw: &str,
        spill_dir: Option<&std::path::Path>,
    ) -> String {
        let configured = self.config.conversation.tool_output_max_tokens;
        let window_cap = (self.active_context_window() as usize / 4).max(256);
        let effective = configured.min(window_cap);
        if effective < configured {
            // The clamp engaged: the model's window is small enough that the
            // operator-configured cap can't fit. Log it so the reduced ceiling
            // is observable rather than a silent override.
            tracing::debug!(
                configured,
                effective,
                context_window = self.active_context_window(),
                model = %self.config.llm.model,
                "tool output cap clamped to model context window"
            );
        }
        let redacted = self.maybe_redact(raw.to_string());
        let truncated = truncate_output(&redacted, effective);
        if truncated.len() >= redacted.len() {
            // Fit the cap — nothing was dropped, so there is nothing to
            // recover and no reason to touch the disk.
            return truncated;
        }
        let spilled = spill_dir.and_then(|dir| spill_tool_result(dir, &redacted));
        compose_spilled_result(truncated, redacted.len(), spilled)
    }

    /// Push a message to history and persist it to SQLite for crash recovery.
    pub(super) async fn persist_message(&mut self, msg: Message) {
        self.persist_messages(vec![msg]).await;
    }

    /// Push a batch of messages to history and persist them in ONE SQLite
    /// transaction, in order. The single-transaction flush matters on tool
    /// turns: an assistant message plus N tool results through the one-row
    /// path costs N+1 `BEGIN IMMEDIATE` transactions and N+1 HMAC chain-head
    /// reads; batched it is one of each, with byte-identical rows.
    pub(super) async fn persist_messages(&mut self, mut msgs: Vec<Message>) {
        if msgs.is_empty() {
            return;
        }
        // Once storage is exhausted, stop persisting for the rest of the turn.
        //
        // A turn writes the assistant message carrying `tool_calls` in one
        // batch and its `tool_result`s in a later one. If the first batch
        // faulted and a later one squeezed through (a freed page, a smaller
        // payload), history would hold tool results with no preceding tool
        // use — a shape Anthropic and OpenAI both reject, and one that would
        // outlive the aborted turn and poison every subsequent one. Dropping
        // the whole turn's writes keeps history well-formed.
        if self.storage_fault.is_some() {
            return;
        }
        let session_id = self.session.meta.id.clone();
        let drafts: Vec<crate::db::SessionEventDraft> =
            msgs.iter().map(draft_from_message).collect();
        let mut fault: Option<String> = None;
        {
            let guard = self.db_guard().await;
            if let Some(ref db) = *guard {
                // Deliberately NOT wrapped in `with_transient_io_retry`: an
                // IOERR surfacing from the COMMIT is ambiguous — the batch may
                // already be durable — and re-running the append would insert
                // every draft a second time into the HMAC-chained log (the
                // chain re-reads the tail, so the duplicates even verify).
                // Only idempotent writers (the whole-file session auto-save)
                // may retry.
                match db.append_session_events(&session_id, &drafts).await {
                    // Stamp the durable log positions onto the in-memory copies
                    // so the working set stays mappable to the log (compaction
                    // boundaries, fork offsets) without a re-read.
                    Ok(seqs) => {
                        for (msg, seq) in msgs.iter_mut().zip(seqs) {
                            msg.seq = Some(seq);
                        }
                    }
                    Err(e) => {
                        // Name the cause: an undifferentiated warn sent
                        // operators hunting for disk space when the actual
                        // problem was another process holding the write lock.
                        if crate::db::is_lock_contention(&e) {
                            warn!(
                                "Failed to persist message batch: SQLite lock contention \
                                 (another borg process holds the write lock). The turn's \
                                 messages stay in memory but are NOT durable: {e}"
                            );
                        } else {
                            warn!("Failed to persist message batch to SQLite: {e}");
                        }
                        if crate::db::is_storage_exhausted(&e) {
                            fault = Some(storage_fault_message(&e));
                        }
                    }
                }
            }
        }
        if let Some(msg) = fault {
            self.storage_fault.get_or_insert(msg);
            // Do NOT extend in-memory history: the next turn's prompt would
            // then contain a message SQLite never saw, and the divergence
            // would survive into compaction. Losing it here is honest.
            return;
        }
        self.history.extend(msgs);
    }
}

/// Filename prefix for conversation-level tool spills. Distinct from
/// `run_shell`'s so the two reapers never delete each other's files.
const TOOL_SPILL_PREFIX: &str = "tool-spill";

/// Attach the recovery pointer to a truncated result.
///
/// `spilled` is `None` when the spill could not be written, in which case the
/// truncated text stands alone — naming a file that does not exist would send
/// the model to read a path that errors, which is worse than saying nothing.
///
/// Split out from [`Agent::truncate_and_redact`] so the wording and the
/// no-spill fallback are testable without touching the filesystem or the
/// process-global `BORG_HOME`.
fn compose_spilled_result(
    truncated: String,
    full_len: usize,
    spilled: Option<std::path::PathBuf>,
) -> String {
    match spilled {
        Some(path) => format!(
            "{truncated}\n\n[full result ({full_len} bytes) saved to {}; read or grep that file \
             instead of re-running the tool]",
            path.display()
        ),
        None => truncated,
    }
}

#[cfg(test)]
mod spill_compose_tests {
    use super::compose_spilled_result;

    /// A failed spill must leave the truncated text exactly as it was.
    /// Appending a pointer to a file that was never written would send the
    /// model to read a path that errors — worse than saying nothing, because
    /// it costs a tool call to discover.
    #[test]
    fn a_failed_spill_adds_no_pointer() {
        let out = compose_spilled_result("head…tail".to_string(), 9_999, None);
        assert_eq!(out, "head…tail");
    }

    /// The pointer must carry both halves the model needs: where the file is,
    /// and that reading it beats re-running the tool.
    #[test]
    fn a_written_spill_names_the_path_and_the_recovery_action() {
        let out = compose_spilled_result(
            "head…tail".to_string(),
            9_999,
            Some(std::path::PathBuf::from("/tmp/tool-spill-1-2-result.txt")),
        );
        assert!(out.starts_with("head…tail"), "must keep the truncated text");
        assert!(
            out.contains("/tmp/tool-spill-1-2-result.txt"),
            "must name the path: {out}"
        );
        assert!(
            out.contains("9999 bytes"),
            "must state the full size: {out}"
        );
        assert!(
            out.contains("instead of re-running the tool"),
            "must name the recovery action: {out}"
        );
    }
}

/// `<data_dir>/cache/tool-spill/`. `None` (with a warn) when the data dir
/// cannot be resolved — the result is still delivered truncated.
fn spill_dir() -> Option<std::path::PathBuf> {
    crate::util::spill::spill_dir(TOOL_SPILL_PREFIX, "tool result")
}

/// Write an over-cap tool result (already redacted) into `dir` and return its
/// path. `None` on any failure — the result is still delivered truncated, so a
/// full disk degrades the model's recovery options rather than failing the
/// tool call.
fn spill_tool_result(dir: &std::path::Path, redacted: &str) -> Option<std::path::PathBuf> {
    match crate::util::spill::write_spill_file(dir, TOOL_SPILL_PREFIX, "result", redacted) {
        Ok(path) => Some(path),
        Err(e) => {
            warn!(error = %e, "tool result spill write failed; returning truncated output only");
            None
        }
    }
}

/// Operator-facing explanation for an exhausted-storage write failure. Names
/// the cause and the fix — "write failed" alone sends people hunting, and
/// telling someone with a flaky disk to "free space" sends them hunting in
/// the wrong place: the exhausted class covers both `SQLITE_FULL` and
/// `SQLITE_IOERR`, which have different remedies.
fn storage_fault_message(err: &anyhow::Error) -> String {
    if crate::db::is_transient_io_error(err) {
        format!(
            "Turn aborted: a disk I/O error prevented saving the assistant's reply. \
             The volume holding ~/.borg/borg.db may be failing, full of bad blocks, \
             or unmounted — check the disk before retrying. ({err})"
        )
    } else {
        format!(
            "Turn aborted: session storage is full, so the assistant's reply was NOT saved. \
             Free space on the volume holding ~/.borg/borg.db and retry. ({err})"
        )
    }
}

/// Project a [`Message`] onto the durable-log row shape. Shared by the
/// single and batched persist paths so the two cannot drift.
fn draft_from_message(msg: &Message) -> crate::db::SessionEventDraft {
    let role = match msg.role {
        crate::types::Role::System => "system",
        crate::types::Role::User => "user",
        crate::types::Role::Assistant => "assistant",
        crate::types::Role::Tool => "tool",
    };
    let tool_calls_json = msg
        .tool_calls
        .as_ref()
        .and_then(|tc| serde_json::to_string(tc).ok());
    let (content_text, content_parts_json) = match &msg.content {
        Some(crate::types::MessageContent::Parts(parts)) => {
            let full = msg
                .content
                .as_ref()
                .map(crate::types::MessageContent::full_text);
            // The plain-text `full` above is the load-bearing copy; the parts
            // blob is a replay nicety, so a serialization failure loses
            // fidelity rather than the message.
            let parts_json = serde_json::to_string(parts).ok();
            (full, parts_json)
        }
        _ => (msg.text_content().map(str::to_string), None),
    };
    crate::db::SessionEventDraft {
        role: role.to_string(),
        event_kind: None,
        content: content_text,
        tool_calls_json,
        tool_call_id: msg.tool_call_id.clone(),
        timestamp: msg.timestamp.clone(),
        content_parts_json,
        event_json: None,
    }
}

#[cfg(test)]
mod storage_fault_message_tests {
    use super::storage_fault_message;

    /// The exhausted class covers both full-disk and I/O errors, but the
    /// remedies differ: telling someone with a failing disk to "free space"
    /// misdirects the recovery. The message must name the actual cause.
    #[test]
    fn names_the_actual_cause_full_vs_io() {
        let full = anyhow::Error::new(libsql::Error::SqliteFailure(
            13,
            "database or disk is full".to_string(),
        ));
        let msg = storage_fault_message(&full);
        assert!(
            msg.contains("storage is full") && msg.contains("Free space"),
            "full-disk fault must prescribe freeing space: {msg}"
        );
        assert!(
            !msg.contains("I/O error"),
            "full-disk fault must not claim an I/O error: {msg}"
        );

        let ioerr = anyhow::Error::new(libsql::Error::SqliteFailure(
            778, // SQLITE_IOERR_WRITE
            "disk I/O error".to_string(),
        ));
        let msg = storage_fault_message(&ioerr);
        assert!(
            msg.contains("disk I/O error") && msg.contains("check the disk"),
            "I/O fault must point at the disk, not free space: {msg}"
        );
        assert!(
            !msg.contains("Free space"),
            "I/O fault must not prescribe freeing space: {msg}"
        );
    }
}
