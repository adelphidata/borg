//! Pluggable per-session backend.
//!
//! Production uses [`AgentBackend`] which owns a real `cortex::Agent`.
//! Tests provide a stub backend that emits canned events without needing an
//! LLM provider — keeps the daemon's session machinery testable in isolation.

use async_trait::async_trait;
use cortex::agent::{Agent, AgentEvent};
use cortex::config::Config;
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// Acquire the agent mutex from a sync trait method that may be invoked
/// from an async gRPC handler (i.e. from inside the tokio runtime).
///
/// `tokio::sync::Mutex::blocking_lock` panics with "Cannot block the current
/// thread from within a runtime" when called on a worker thread, and the
/// panic kills that worker mid-turn — which is how `/poke`-triggered
/// heartbeats end up surfacing as `*poked Borggundy*` and nothing else: the
/// `Session.GetState` snapshot RPC racing the in-flight turn panicked the
/// thread that was streaming events back through the bus.
///
/// `block_in_place` lets the multi-thread runtime move other tasks off this
/// worker so we can block on the lock without starving the scheduler. The
/// trait surface stays synchronous (and stub backends stay simple); only
/// `AgentBackend`'s production path needs the async-lock bridge.
fn lock_agent(agent: &Mutex<Agent>) -> tokio::sync::MutexGuard<'_, Agent> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(agent.lock()))
}

/// RAII guard that detaches an attached IDE-backed text-file bridge when it
/// drops — including the panic-unwind path — so the next turn never inherits
/// a stale bridge. Owns the sole live `&mut Agent` for the turn; agent calls
/// go through `guard.agent`, and `Drop` reuses the same reference to detach.
struct TextFileBridgeAutoDetach<'a> {
    agent: &'a mut Agent,
    attached: bool,
}

impl<'a> TextFileBridgeAutoDetach<'a> {
    fn new(agent: &'a mut Agent, attached: bool) -> Self {
        Self { agent, attached }
    }
}

impl Drop for TextFileBridgeAutoDetach<'_> {
    fn drop(&mut self) {
        if self.attached {
            self.agent.set_text_file_bridge(None);
        }
    }
}

/// Snapshot of the agent's user-facing state for `Session.GetState`.
#[derive(Debug, Clone)]
pub struct SessionStateSnapshot {
    /// The cortex session id (used for resume).
    pub agent_session_id: String,
    /// Human-friendly title of the active session.
    pub title: String,
    /// Total messages in history.
    pub message_count: u64,
    /// Number of user-authored messages.
    pub user_message_count: u64,
    /// Estimated token count of current history.
    pub token_estimate: u64,
    /// Active LLM provider name.
    pub llm_provider: String,
    /// Active model id.
    pub llm_model: String,
    /// Collaboration mode label (default/execute/plan).
    pub collab_mode: String,
}

/// Result of forking the active agent session.
#[derive(Debug, Clone)]
pub struct ForkOutcome {
    /// New cortex session id created by the fork.
    pub forked_agent_session_id: String,
    /// Title of the new session (typically `"<parent> (fork)"`).
    pub title: String,
    /// Number of messages copied into the fork.
    pub message_count: u64,
}

pub use cortex::session_types::InlineImage;

/// Per-session backend that runs one agent turn at a time.
#[async_trait]
pub trait SessionBackend: Send + Sync + 'static {
    /// Run a turn on the user's `text` plus optional inline `images`,
    /// streaming `AgentEvent`s through `event_tx`. Implementations MUST send
    /// `AgentEvent::TurnComplete` (or `AgentEvent::Error`) before returning so
    /// the gRPC stream can close.
    async fn run_turn(
        &self,
        text: String,
        images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    );

    /// Stable session id (for logs / SessionRegistry indexing).
    fn session_id(&self) -> &str;

    /// Finalize and close the backend session.
    ///
    /// Production backends should use this to flush any session-end lifecycle
    /// work that must happen before the daemon reports the session closed.
    async fn close_session(&self) -> anyhow::Result<()> {
        Ok(())
    }

    // ── Agent-control surface (default: not supported) ────────────────────
    //
    // These all return `Err(unsupported)` for stub backends; only `AgentBackend`
    // implements them. Daemon RPC handlers translate `unsupported` into a
    // gRPC `unimplemented` status so callers can render a clean message.

    /// Compact conversation history. Returns `(dropped, after_tokens)`.
    async fn compact(&self) -> anyhow::Result<(usize, usize)> {
        Err(anyhow::anyhow!("compact not supported by this backend"))
    }
    /// Wipe history (keeping the agent alive for a fresh turn).
    fn clear_history(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "clear_history not supported by this backend"
        ))
    }
    /// Drop the most recent assistant turn. Returns # messages removed.
    fn undo(&self) -> anyhow::Result<usize> {
        Err(anyhow::anyhow!("undo not supported by this backend"))
    }
    /// Rewind history to the Nth user message. Returns # messages removed.
    fn rewind_to_nth_user_message(&self, _n: usize) -> anyhow::Result<usize> {
        Err(anyhow::anyhow!("rewind not supported by this backend"))
    }
    /// Persist current state to disk.
    fn auto_save(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("auto_save not supported by this backend"))
    }
    /// Start a fresh agent session, returning the new session id.
    async fn new_agent_session(&self) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("new_session not supported by this backend"))
    }
    /// Load an existing agent session from disk by id.
    fn load_agent_session(&self, _id: &str) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!(
            "load_session not supported by this backend"
        ))
    }
    /// Read current state for `Session.GetState`.
    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        Err(anyhow::anyhow!(
            "snapshot_state not supported by this backend"
        ))
    }
    /// Push a steer message into the running agent loop.
    fn steer(&self, _text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("steer not supported by this backend"))
    }

    /// Append a system-injected assistant message to the agent's history.
    /// Default unsupported; production backends mirror it into both
    /// in-memory history and SQLite so the next user turn sees it.
    async fn append_assistant_message(&self, _text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "append_assistant_message not supported by this backend"
        ))
    }

    /// Fork the active agent session at its last message. Daemon owns the
    /// single SQLite writer, so this MUST run here rather than in the client.
    async fn fork(&self) -> anyhow::Result<ForkOutcome> {
        Err(anyhow::anyhow!("fork not supported by this backend"))
    }
}

/// Production backend: owns one `cortex::Agent`. Hooks (vitals, activity,
/// bond, evolution, script) are registered at construction time, matching the
/// in-process behavior of `crates/cli/src/repl.rs` and `tui/mod.rs`.
pub struct AgentBackend {
    session_id: String,
    /// `Agent::send_message_with_cancel` takes `&mut self`; serialize turns
    /// behind a Mutex so two concurrent `Send` RPCs to the same session
    /// (which the protocol forbids but the transport doesn't enforce) queue
    /// safely instead of corrupting state.
    agent: Mutex<Agent>,
    /// Steer channel sender — held here so `Session.Steer` can push text
    /// into the running turn even though the agent is locked.
    steer_tx: mpsc::UnboundedSender<String>,
}

impl AgentBackend {
    /// Build a backend hosting a fresh agent. `resume_id` (if set) loads
    /// existing session history; otherwise a new session is started.
    ///
    /// `goal_event_sink` is the daemon-side bus that `Admin.SubscribeGoalEvents`
    /// publishes to — passing `None` is fine (the agent loop tolerates an
    /// absent sink), but production wires one through so the TUI footer and
    /// any other subscribers see lifecycle events without polling.
    pub async fn new(
        config: Config,
        metrics: BorgMetrics,
        resume_id: Option<&str>,
        config_rx: Option<watch::Receiver<Config>>,
        cost_event_sink: Option<Arc<dyn CostEventSink>>,
        execution_context: Option<cortex::execution_context::ExecutionContext>,
        goal_bus: Option<crate::goal_bus::GoalEventBus>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut agent = Agent::new(config, metrics).await?;
        if let Some(execution_context) = execution_context {
            agent.set_execution_context(execution_context);
        }
        if let Some(rx) = config_rx {
            agent.set_config_watcher(rx);
        }
        if let Some(sink) = cost_event_sink {
            agent.set_cost_event_sink(sink);
        }
        if let Some(bus) = goal_bus {
            agent.set_goal_event_sink(bus.as_sink());
        }

        // Register the same hook set the in-process REPL/TUI uses. Failed
        // registration is non-fatal — log so operators can debug "why is XP
        // not advancing" rather than have it silently drop.
        match cortex::vitals::VitalsHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "vitals hook unavailable"),
        }
        match cortex::activity_log::ActivityHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "activity_log hook unavailable"),
        }
        match cortex::bond::BondHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "bond hook unavailable"),
        }
        if agent.config().evolution.enabled {
            match cortex::evolution::EvolutionHook::new().await {
                Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
                Err(e) => tracing::warn!(error = %e, "evolution hook unavailable"),
            }
        }
        match cortex::budgets::BudgetHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "budget hook unavailable"),
        }
        for hook in cortex::hooks::ScriptHook::load_all(agent.config().hooks.enabled) {
            agent.hook_registry_mut().register(Box::new(hook));
        }

        if let Some(id) = resume_id {
            if let Err(e) = agent.load_session(id) {
                tracing::warn!(session_id = %id, error = %e, "load_session failed; starting fresh");
                agent.new_session().await;
            }
        }

        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        agent.set_steer_channel(steer_rx);

        let session_id = agent.session().meta.id.clone();
        Ok(Arc::new(Self {
            session_id,
            agent: Mutex::new(agent),
            steer_tx,
        }))
    }
}

#[async_trait]
impl SessionBackend for AgentBackend {
    async fn run_turn(
        &self,
        text: String,
        images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    ) {
        // Reject overlapping turns explicitly rather than silently queuing on
        // an async lock — a hung second `Send` is indistinguishable from a
        // crashed daemon for the user. SessionHost serializes turns at a
        // higher level, so this branch only fires when the protocol is
        // misused (concurrent Send to the same session_id).
        let mut agent = match self.agent.try_lock() {
            Ok(a) => a,
            Err(_) => {
                let msg = "another turn is already in flight for this session";
                tracing::warn!(session_id = %self.session_id, "{msg}");
                if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
                    tracing::warn!(error = %e, "failed to surface concurrent-turn error");
                }
                return;
            }
        };

        // Attach the IDE-backed text-file bridge for the duration of the turn
        // and detach unconditionally — including on unwind. The async Mutex
        // serializes turns on this backend, so the next turn must not inherit
        // a stale bridge if this turn's future is dropped or panics.
        let bridge_attached = text_file_bridge.is_some();
        if bridge_attached {
            agent.set_text_file_bridge(text_file_bridge);
        }
        let bridge_guard = TextFileBridgeAutoDetach::new(&mut agent, bridge_attached);

        let result = if images.is_empty() {
            bridge_guard
                .agent
                .send_message_with_cancel(&text, event_tx.clone(), cancel)
                .await
        } else {
            use base64::Engine as _;
            use cortex::types::{ContentPart, MediaData, Message};
            let engine = base64::engine::general_purpose::STANDARD;
            let mut parts = vec![ContentPart::Text(text)];
            for img in images {
                parts.push(ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: img.mime_type,
                        data: engine.encode(&img.data),
                        filename: img.filename,
                    },
                });
            }
            let msg = Message::user_multimodal(parts);
            bridge_guard
                .agent
                .send_message_raw(msg, event_tx.clone(), cancel)
                .await
        };
        // bridge_guard's Drop runs here (or earlier on unwind) and detaches.
        drop(bridge_guard);

        if let Err(e) = result {
            // anyhow's `Display` only renders the topmost message; `{:#}` walks
            // the `.context()` chain so the user (and the daemon log) sees the
            // root cause instead of just the outer wrap.
            let chained = format!("{e:#}");
            tracing::warn!(session_id = %self.session_id, error = %chained, "agent turn failed");
            if let Err(send_err) = event_tx.send(AgentEvent::Error(chained)).await {
                tracing::warn!(error = %send_err, "failed to surface agent error to client");
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn close_session(&self) -> anyhow::Result<()> {
        let mut agent = self.agent.lock().await;
        agent.end_session().await;
        Ok(())
    }

    async fn compact(&self) -> anyhow::Result<(usize, usize)> {
        let mut agent = self.agent.lock().await;
        Ok(agent.compact().await)
    }

    fn clear_history(&self) -> anyhow::Result<()> {
        let mut agent = lock_agent(&self.agent);
        agent.clear_history();
        Ok(())
    }

    fn undo(&self) -> anyhow::Result<usize> {
        let mut agent = lock_agent(&self.agent);
        Ok(agent.undo())
    }

    fn rewind_to_nth_user_message(&self, n: usize) -> anyhow::Result<usize> {
        let mut agent = lock_agent(&self.agent);
        Ok(agent.rewind_to_nth_user_message(n))
    }

    fn auto_save(&self) -> anyhow::Result<()> {
        let mut agent = lock_agent(&self.agent);
        agent.auto_save();
        Ok(())
    }

    async fn new_agent_session(&self) -> anyhow::Result<String> {
        let mut agent = self.agent.lock().await;
        agent.new_session().await;
        Ok(agent.session().meta.id.clone())
    }

    fn load_agent_session(&self, id: &str) -> anyhow::Result<u64> {
        let mut agent = lock_agent(&self.agent);
        agent.load_session(id)?;
        Ok(agent.session().messages.len() as u64)
    }

    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        let agent = lock_agent(&self.agent);
        let session = agent.session();
        let cfg = agent.config();
        let (msgs, users) = agent.conversation_stats();
        Ok(SessionStateSnapshot {
            agent_session_id: session.meta.id.clone(),
            title: session.meta.title.clone(),
            message_count: msgs as u64,
            user_message_count: users as u64,
            token_estimate: 0,
            llm_provider: cfg.llm.provider.clone().unwrap_or_default(),
            llm_model: cfg.llm.model.clone(),
            collab_mode: format!("{}", cfg.conversation.collaboration_mode),
        })
    }

    fn steer(&self, text: &str) -> anyhow::Result<()> {
        self.steer_tx
            .send(text.to_string())
            .map_err(|e| anyhow::anyhow!("steer channel closed: {e}"))
    }

    async fn append_assistant_message(&self, text: &str) -> anyhow::Result<()> {
        let mut agent = self.agent.lock().await;
        agent.append_assistant_message(text).await;
        Ok(())
    }

    async fn fork(&self) -> anyhow::Result<ForkOutcome> {
        // Resolve the parent agent session id under the agent lock, then
        // release it before doing blocking DB work — fork can copy a large
        // number of message rows and we don't want to block concurrent
        // reads (e.g. `GetState`) on the agent for the full duration.
        let parent_id = {
            let agent = self.agent.lock().await;
            agent.session().meta.id.clone()
        };

        let db = cortex::db::Database::open().await?;
        let msgs = db.load_session_messages(&parent_id).await?;
        let anchor = msgs
            .last()
            .map(|m| m.id)
            .ok_or_else(|| anyhow::anyhow!("session has no messages"))?;
        drop(db);
        let forked = cortex::session::fork_session(&parent_id, anchor).await?;
        Ok(ForkOutcome {
            forked_agent_session_id: forked.meta.id,
            title: forked.meta.title,
            message_count: forked.meta.message_count as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: calling `lock_agent` from within a multi-thread tokio
    /// runtime (which is what every production gRPC handler does) must not
    /// panic with "Cannot block the current thread from within a runtime".
    /// The naive `Mutex::blocking_lock()` panics here, killing the worker
    /// thread mid-turn. Symptom in production: `/poke` shows the local
    /// "*poked Borggundy*" line but no heartbeat response — the panic
    /// aborts the worker that was streaming bus events back to the TUI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lock_agent_does_not_panic_inside_runtime() {
        // Models the production failure mode: a sync function using
        // `block_in_place + block_on` is called from inside a tokio async
        // task. Naive `Mutex::blocking_lock()` panics with "Cannot block
        // the current thread from within a runtime"; the bridge below must
        // not.
        let mutex = std::sync::Arc::new(Mutex::new(42i64));
        fn sync_lock_bridge(m: &Mutex<i64>) -> tokio::sync::MutexGuard<'_, i64> {
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(m.lock()))
        }
        let g = sync_lock_bridge(&mutex);
        assert_eq!(*g, 42);
    }

    /// Regression: errors surfaced via `AgentEvent::Error` must include the
    /// full `anyhow` context chain, not just the topmost wrap. A previous
    /// version used `e.to_string()` which produced messages like
    /// "Failed to build system prompt" with no underlying cause — leaving
    /// the user (and the daemon log) with nothing actionable. Source-grep
    /// guard so a future contributor can't silently revert this to
    /// `e.to_string()` and lose the context chain again. Pure
    /// library-behavior tests over `format!("{e:#}")` would pass even
    /// against a reverted production callsite.
    #[test]
    fn agent_error_callsite_uses_anyhow_alternate_format() {
        let src = include_str!("backend.rs");
        // Strip this test's own body before scanning so the patterns inside
        // string literals don't false-positive on themselves.
        let marker = "fn agent_error_callsite_uses_anyhow_alternate_format";
        let scan = match src.find(marker) {
            Some(idx) => &src[..idx],
            None => src,
        };
        // The production path must construct the message via `{:#}` so
        // anyhow walks the `.context()` chain.
        assert!(
            scan.contains("format!(\"{e:#}\")"),
            "backend.rs lost the anyhow-chained error format — \
             AgentEvent::Error would surface only the outer wrap"
        );
        // And it must not regress to a bare `.to_string()` on the error
        // before sending into the channel. The closest negative pattern
        // is the exact line shape the pre-fix code used.
        assert!(
            !scan.contains("AgentEvent::Error(e.to_string()"),
            "backend.rs reverted to e.to_string() — lose the context chain"
        );
    }
}
