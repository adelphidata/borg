//! Pluggable per-session backend.
//!
//! Production uses [`AgentBackend`] which owns a real `borg_core::Agent`.
//! Tests provide a stub backend that emits canned events without needing an
//! LLM provider — keeps the daemon's session machinery testable in isolation.

use async_trait::async_trait;
use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::Config;
use borg_core::telemetry::BorgMetrics;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// Snapshot of the agent's user-facing state for `Session.GetState`.
#[derive(Debug, Clone)]
pub struct SessionStateSnapshot {
    /// The borg_core session id (used for resume).
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

/// Per-session backend that runs one agent turn at a time.
#[async_trait]
pub trait SessionBackend: Send + Sync + 'static {
    /// Run a turn on the user's `text`, streaming `AgentEvent`s through
    /// `event_tx`. Implementations MUST send `AgentEvent::TurnComplete` (or
    /// `AgentEvent::Error`) before returning so the gRPC stream can close.
    async fn run_turn(
        &self,
        text: String,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    );

    /// Stable session id (for logs / SessionRegistry indexing).
    fn session_id(&self) -> &str;

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
    fn new_agent_session(&self) -> anyhow::Result<String> {
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
}

/// Production backend: owns one `borg_core::Agent`. Hooks (vitals, activity,
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
    pub fn new(
        config: Config,
        metrics: BorgMetrics,
        resume_id: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut agent = Agent::new(config, metrics)?;

        // Register the same hook set the in-process REPL/TUI uses. Failed
        // registration is non-fatal — log so operators can debug "why is XP
        // not advancing" rather than have it silently drop.
        match borg_core::vitals::VitalsHook::new() {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "vitals hook unavailable"),
        }
        match borg_core::activity_log::ActivityHook::new() {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "activity_log hook unavailable"),
        }
        match borg_core::bond::BondHook::new() {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "bond hook unavailable"),
        }
        if agent.config().evolution.enabled {
            match borg_core::evolution::EvolutionHook::new() {
                Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
                Err(e) => tracing::warn!(error = %e, "evolution hook unavailable"),
            }
        }
        for hook in borg_core::hooks::ScriptHook::load_all(agent.config().hooks.enabled) {
            agent.hook_registry_mut().register(Box::new(hook));
        }

        if let Some(id) = resume_id {
            if let Err(e) = agent.load_session(id) {
                tracing::warn!(session_id = %id, error = %e, "load_session failed; starting fresh");
                agent.new_session();
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
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
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
        if let Err(e) = agent
            .send_message_with_cancel(&text, event_tx.clone(), cancel)
            .await
        {
            tracing::warn!(session_id = %self.session_id, error = %e, "agent turn failed");
            if let Err(send_err) = event_tx.send(AgentEvent::Error(e.to_string())).await {
                tracing::warn!(error = %send_err, "failed to surface agent error to client");
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn compact(&self) -> anyhow::Result<(usize, usize)> {
        let mut agent = self.agent.lock().await;
        Ok(agent.compact().await)
    }

    fn clear_history(&self) -> anyhow::Result<()> {
        let mut agent = self.agent.blocking_lock();
        agent.clear_history();
        Ok(())
    }

    fn undo(&self) -> anyhow::Result<usize> {
        let mut agent = self.agent.blocking_lock();
        Ok(agent.undo())
    }

    fn rewind_to_nth_user_message(&self, n: usize) -> anyhow::Result<usize> {
        let mut agent = self.agent.blocking_lock();
        Ok(agent.rewind_to_nth_user_message(n))
    }

    fn auto_save(&self) -> anyhow::Result<()> {
        let mut agent = self.agent.blocking_lock();
        agent.auto_save();
        Ok(())
    }

    fn new_agent_session(&self) -> anyhow::Result<String> {
        let mut agent = self.agent.blocking_lock();
        agent.new_session();
        Ok(agent.session().meta.id.clone())
    }

    fn load_agent_session(&self, id: &str) -> anyhow::Result<u64> {
        let mut agent = self.agent.blocking_lock();
        agent.load_session(id)?;
        Ok(agent.session().messages.len() as u64)
    }

    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        let agent = self.agent.blocking_lock();
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
}
