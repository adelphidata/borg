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

        let session_id = agent.session().meta.id.clone();
        Ok(Arc::new(Self {
            session_id,
            agent: Mutex::new(agent),
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
}
