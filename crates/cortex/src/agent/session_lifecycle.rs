//! Session lifecycle: load, restart, end, autosave, history accessors.
//!
//! Pulled out of `agent/mod.rs`. These are the operator-facing entry points
//! that own session boundaries plus a handful of read-only accessors. They
//! stay on `impl Agent` so the public surface (`pub fn`) is unchanged.

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{instrument, warn};

use crate::conversation::{
    compact_history, compact_history_v2, history_tokens, rewind_to_nth_user, undo_last_turn,
};
use crate::hooks::{HookData, HookPoint};
use crate::session::Session;
use crate::types::Message;

use super::{spawn_logged, Agent, AgentEvent};

impl Agent {
    /// Inject a message directly into the conversation history.
    pub fn inject_history_message(&mut self, msg: Message) {
        self.history.push(msg);
    }

    /// Restore a session's history into this agent.
    pub fn load_session(&mut self, id: &str) -> Result<()> {
        let session = Session::load(id)?;
        self.history = session.messages.clone();
        self.session = session;
        Ok(())
    }

    /// Start a new session, clearing history.
    pub fn new_session(&mut self) {
        // Fire SessionEnd for the previous session if any turns were executed
        if self.turn_count > 0 {
            let hook_ctx = self.hook_ctx(
                HookPoint::SessionEnd,
                HookData::SessionEnd {
                    session_id: self.session.meta.id.clone(),
                    total_turns: self.turn_count,
                },
            );
            self.hook_registry.dispatch(&hook_ctx);
            self.flush_short_term_memory();
        }

        self.history.clear();
        self.session = Session::new();
        self.short_term_memory = crate::memory::short_term_memory::ShortTermMemory::new(
            self.session.meta.id.clone(),
            2000,
        );

        // Fire SessionStart for the new session
        let hook_ctx = self.hook_ctx(
            HookPoint::SessionStart,
            HookData::SessionStart {
                session_id: self.session.meta.id.clone(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx);
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
    pub fn flush_short_term_memory(&self) {
        let facts = self.short_term_memory.facts_as_text();
        if facts.trim().is_empty() {
            return;
        }
        let guard = self.db_guard();
        if let Err(e) = crate::memory::consolidation::flush_short_term_to_daily_with_optional_db(
            guard.as_ref(),
            &facts,
        ) {
            warn!("flush_short_term_memory failed: {e}");
        }
    }

    /// Signal that this agent's owning context is ending (e.g. CLI shutdown,
    /// gateway handler finishing). Fires SessionEnd and flushes short-term
    /// memory even when no `new_session()` call follows. Safe to call multiple
    /// times; a no-op if no turns were executed.
    pub fn end_session(&mut self) {
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
        self.hook_registry.dispatch(&hook_ctx);
        self.flush_short_term_memory();
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

    /// Compact conversation history using LLM summarization, returning (before_tokens, after_tokens).
    #[instrument(skip_all, fields(session_id = %self.session.meta.id))]
    pub async fn compact(&mut self) -> (usize, usize) {
        let before = history_tokens(&self.history);
        let llm = match self.make_llm_client_from_config(&self.config) {
            Ok(l) => l,
            Err(_) => return (before, before),
        };
        let max_hist = self.config.conversation.max_history_tokens;
        let protect_first_n = self.config.conversation.protect_first_n;
        if protect_first_n > 0 {
            if self.previous_summary.is_none() {
                self.previous_summary =
                    crate::conversation::extract_last_compaction_summary(&self.history);
            }
            compact_history_v2(
                &mut self.history,
                max_hist,
                protect_first_n,
                &mut self.previous_summary,
                &llm,
            )
            .await;
        } else {
            compact_history(&mut self.history, max_hist, &llm).await;
        }
        let after = history_tokens(&self.history);
        (before, after)
    }

    /// Clear all conversation history.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Undo the last agent turn: remove everything back to the last user message.
    /// Returns the number of messages removed, or 0 if nothing to undo.
    pub fn undo(&mut self) -> usize {
        undo_last_turn(&mut self.history)
    }

    /// Rewind conversation to the Nth user message (0-indexed, oldest-first).
    /// Removes that message and everything after it.
    /// Returns the number of messages removed.
    pub fn rewind_to_nth_user_message(&mut self, n: usize) -> usize {
        rewind_to_nth_user(&mut self.history, n)
    }

    /// Returns (message_count, estimated_token_count) for the current session.
    pub fn conversation_stats(&self) -> (usize, usize) {
        (self.history.len(), history_tokens(&self.history))
    }

    /// Shut down all sub-agents (called on cancellation).
    pub(super) fn shutdown_sub_agents(&mut self) {
        if let Some(ref mut ctrl) = self.agent_control {
            ctrl.shutdown_all();
        }
    }

    /// Collect completed sub-agent results and persist them as tool result messages.
    pub(super) fn drain_sub_agent_completions(&mut self) {
        if let Some(ref mut ctrl) = self.agent_control {
            for completion in ctrl.drain_completions() {
                let ctx = format!(
                    "[Sub-agent \"{}\" (id: {}) status: {}]\n{}",
                    completion.nickname,
                    completion.agent_id,
                    completion.status.as_str(),
                    completion
                        .final_response
                        .as_deref()
                        .unwrap_or("(no output)")
                );
                self.persist_message(Message::tool_result("sub-agent", &ctx));
            }
        }
    }

    /// Drain pending steer messages from the user and inject them into history.
    #[instrument(skip_all)]
    pub(super) async fn drain_steers(&mut self, event_tx: &mpsc::Sender<AgentEvent>) {
        // Collect all steers first to avoid borrow conflict with self.persist_message
        let steers: Vec<String> = if let Some(ref mut steer_rx) = self.steer_rx {
            let mut collected = Vec::new();
            while let Ok(text) = steer_rx.try_recv() {
                collected.push(text);
            }
            collected
        } else {
            return;
        };

        for steer_text in steers {
            self.persist_message(Message::user(&steer_text));
            let _ = event_tx
                .send(AgentEvent::SteerReceived { text: steer_text })
                .await;
        }
    }

    /// One-time setup before entering the agent loop: ghost commit, project doc caching,
    /// and background indexing of extra memory paths and past sessions.
    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    pub(super) async fn prepare_loop(&mut self) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Create ghost commit for coding safety (lazy, one-time)
        if self.ghost_commit.is_none() {
            if let Some(ref root) = self.git_repo_root {
                match crate::git::create_ghost_commit(root).await {
                    Ok(gc) => {
                        tracing::info!(commit_id = %gc.commit_id, "Created ghost commit for session");
                        if let Ok(guard) = self.db.lock() {
                            if let Some(ref db) = *guard {
                                crate::activity_log::log_activity(
                                    db,
                                    "info",
                                    "session",
                                    &format!("Ghost commit created: {}", gc.commit_id),
                                );
                            }
                        }
                        self.ghost_commit = Some(gc);
                    }
                    Err(e) => tracing::debug!("Skipping ghost commit: {e}"),
                }
            }
        }

        // Cache project docs on first run (the system-prompt builder also
        // calls this lazily; running it here once outside that hot path
        // means the first build_system_prompt call doesn't pay for IO).
        let _ = self.caches.project_docs_or_load(|| {
            crate::project_doc::discover_project_docs(self.execution_context.workspace_cwd())
                .ok()
                .flatten()
        });

        // Background: index extra paths and pending sessions on first run
        if self.config.memory.embeddings.enabled {
            if !self.config.memory.extra_paths.is_empty() {
                let config = Arc::clone(&self.config_arc);
                spawn_logged("embed_extra_paths", async move {
                    let files = crate::memory::scan_extra_paths(
                        &config.memory.extra_paths,
                        &config.security.blocked_paths,
                    );
                    for (filename, path) in files {
                        if let Ok(content) = tokio::fs::read_to_string(&path).await {
                            let _ = crate::memory::embeddings::embed_memory_file_chunked(
                                &config, &filename, &content, "extra",
                            )
                            .await;
                        }
                    }
                });
            }
            static SESSION_INDEX_RUNNING: AtomicBool = AtomicBool::new(false);
            if SESSION_INDEX_RUNNING
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let config = Arc::clone(&self.config_arc);
                spawn_logged("index_sessions", async move {
                    struct IndexGuard;
                    impl Drop for IndexGuard {
                        fn drop(&mut self) {
                            SESSION_INDEX_RUNNING.store(false, Ordering::Release);
                        }
                    }
                    let _guard = IndexGuard;
                    let _ = crate::session_indexer::index_pending_sessions(&config, 10).await;
                });
            }
        }
    }
}
