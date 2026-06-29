//! Session and message-history lifecycle for `Agent`.
//!
//! Extracted from `agent/mod.rs` to keep session boundaries
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
        // Drop the system-prompt input caches (identity, resolved credentials,
        // project docs, skills). Otherwise a long-lived `Agent` reused across a
        // reset would assemble the new session's prompt from the prior session's
        // cached IDENTITY.md / .env values — a stale-context leak. They lazily
        // reload on the next turn.
        self.caches = super::system_prompt_cache::SystemPromptCache::default();

        // Fire SessionStart for the new session
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
    pub fn auto_save(&mut self) {
        self.session.update_from_history(&self.history);
        if let Err(e) = self.session.save() {
            warn!("Failed to auto-save session: {e}");
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

    /// Truncate and optionally redact secrets from raw tool output.
    ///
    /// The configured `tool_output_max_tokens` (default 10k) is a fixed ceiling
    /// that ignores the model — on a small-context model a single 10k-token
    /// tool result is larger than the entire window and crowds out history. We
    /// clamp the per-result cap to a quarter of the active context window so
    /// one result can never dominate, while large-context models keep the
    /// configured ceiling unchanged.
    pub(super) fn truncate_and_redact(&self, raw: &str) -> String {
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
        self.maybe_redact(truncate_output(raw, effective))
    }

    /// Push a message to history and persist it to SQLite for crash recovery.
    pub(super) async fn persist_message(&mut self, mut msg: Message) {
        let session_id = self.session.meta.id.clone();
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
                let parts_json = serde_json::to_string(parts).ok();
                (full, parts_json)
            }
            _ => (msg.text_content().map(str::to_string), None),
        };
        {
            let guard = self.db_guard().await;
            if let Some(ref db) = *guard {
                match db
                    .insert_message(
                        &session_id,
                        role,
                        content_text.as_deref(),
                        tool_calls_json.as_deref(),
                        msg.tool_call_id.as_deref(),
                        msg.timestamp.as_deref(),
                        content_parts_json.as_deref(),
                    )
                    .await
                {
                    // Stamp the durable log position onto the in-memory copy so
                    // the working set stays mappable to the log (compaction
                    // boundaries, fork offsets) without a re-read.
                    Ok(seq) => msg.seq = Some(seq),
                    Err(e) => warn!("Failed to persist message to SQLite: {e}"),
                }
            }
        }
        self.history.push(msg);
    }
}
