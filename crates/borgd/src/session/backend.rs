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

        if let Ok(vitals_hook) = borg_core::vitals::VitalsHook::new() {
            agent.hook_registry_mut().register(Box::new(vitals_hook));
        }
        if let Ok(activity_hook) = borg_core::activity_log::ActivityHook::new() {
            agent.hook_registry_mut().register(Box::new(activity_hook));
        }
        if let Ok(bond_hook) = borg_core::bond::BondHook::new() {
            agent.hook_registry_mut().register(Box::new(bond_hook));
        }
        if agent.config().evolution.enabled {
            if let Ok(evolution_hook) = borg_core::evolution::EvolutionHook::new() {
                agent.hook_registry_mut().register(Box::new(evolution_hook));
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
        let mut agent = self.agent.lock().await;
        if let Err(e) = agent
            .send_message_with_cancel(&text, event_tx.clone(), cancel)
            .await
        {
            tracing::warn!(session_id = %self.session_id, error = %e, "agent turn failed");
            let _ = event_tx.send(AgentEvent::Error(e.to_string())).await;
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }
}
