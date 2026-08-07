//! Trait the gateway uses to hand inbound messages to the daemon's session
//! machinery. Implemented by `borgd` so the gateway never constructs an
//! `Agent` directly. Single-writer per session is enforced by the
//! implementation (the daemon's `SessionRegistry`).

use async_trait::async_trait;
use cortex::agent::AgentEvent;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Who initiated a turn — governs the interrupt policy in
/// [`SessionExecutor::start_turn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnKind {
    /// A real user message. A new one replaces (cancels) a stalled in-flight
    /// reply on the same session — preserves today's "latest message wins"
    /// behavior.
    #[default]
    User,
    /// A system/background-initiated turn (heartbeat, approval greeting,
    /// proactive delivery, async sub-agent completion). Must NEVER abort an
    /// in-flight turn: if the target session is busy it is deferred rather
    /// than interrupting the live turn.
    Internal,
}

/// One inbound message → one turn.
#[derive(Debug, Clone)]
pub struct InboundTurn {
    pub session_key: String,
    pub text: String,
    pub images: Vec<InlineImage>,
    /// Initiator class; defaults to [`TurnKind::User`].
    pub kind: TurnKind,
}

pub use cortex::session_types::InlineImage;

#[async_trait]
pub trait SessionExecutor: Send + Sync + 'static {
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)>;
    async fn cancel(&self, session_key: &str) -> bool;
    async fn cancel_all(&self) -> usize;

    /// How many sessions currently have a turn in flight.
    ///
    /// Used by the restart path to drain before stopping. Without it, an
    /// in-band restart cancels the shutdown token immediately and kills the
    /// very turn that asked for the restart — the request never gets its
    /// reply.
    async fn active_turns(&self) -> usize;
}

pub type SessionExecutorHandle = Arc<dyn SessionExecutor>;

static GLOBAL_EXECUTOR: OnceLock<SessionExecutorHandle> = OnceLock::new();

pub fn set_global(handle: SessionExecutorHandle) {
    if GLOBAL_EXECUTOR.set(handle).is_err() {
        tracing::warn!("session_executor::set_global called twice; ignoring second install");
    }
}

pub fn global() -> Option<&'static SessionExecutorHandle> {
    GLOBAL_EXECUTOR.get()
}
