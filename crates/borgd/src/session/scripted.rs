//! Scripted [`SessionBackend`] for integration tests.
//!
//! Production sessions own a real `cortex::Agent` driven by an LLM. Integration
//! tests need deterministic per-turn output without the LLM in the loop.
//! [`ScriptedBackend`] takes a closure run on every `run_turn` call; the
//! closure spawns a task that emits whatever `AgentEvent`s the test needs and
//! must terminate the turn with `TurnComplete` (or `Error`).
//!
//! Paired with [`ScriptedFactory`], this lets a test fixture spin up the real
//! gRPC services on a per-test UDS without a real LLM. See the ACP E2E test
//! suite at `crates/interface/tests/acp_e2e/` for the canonical consumer.
//!
//! Gated behind `feature = "test-support"` so production binaries cannot
//! accidentally instantiate it.

use crate::grpc::session::SessionFactory;
use crate::session::backend::{InlineImage, SessionBackend, SessionStateSnapshot};
use crate::session::SessionHost;
use anyhow::anyhow;
use async_trait::async_trait;
use cortex::agent::AgentEvent;
use cortex::execution_context::{ExecutionContext, TextFileBridge};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Per-turn script. The closure receives everything `run_turn` was given
/// (including the optional ACP `text_file_bridge`) and must spawn a task that
/// drives the turn to a terminal event. The returned `JoinHandle` lets the
/// backend dispatch the spawn without holding any lock across an `.await`.
pub type TurnScript = Box<
    dyn Send
        + 'static
        + FnMut(
            String,
            Vec<InlineImage>,
            mpsc::Sender<AgentEvent>,
            CancellationToken,
            Option<Arc<dyn TextFileBridge>>,
        ) -> JoinHandle<()>,
>;

/// Stub backend whose per-turn behavior is decided by the test.
///
/// The script is held under a `std::sync::Mutex<Option<TurnScript>>` so it
/// can be `take()`-n-`put back` without dragging the `MutexGuard` across an
/// `.await`. Closure auto-traits don't always include `Sync`, so wrapping the
/// closure in a sync mutex keeps the backend `Send + Sync` regardless.
pub struct ScriptedBackend {
    id: String,
    script: Mutex<Option<TurnScript>>,
    last_text: Mutex<Option<String>>,
    last_bridge_present: Mutex<bool>,
}

impl ScriptedBackend {
    /// Build a new backend with the given `session_id` and per-turn closure.
    pub fn new<F>(id: impl Into<String>, script: F) -> Arc<Self>
    where
        F: Send
            + 'static
            + FnMut(
                String,
                Vec<InlineImage>,
                mpsc::Sender<AgentEvent>,
                CancellationToken,
                Option<Arc<dyn TextFileBridge>>,
            ) -> JoinHandle<()>,
    {
        Arc::new(Self {
            id: id.into(),
            script: Mutex::new(Some(Box::new(script))),
            last_text: Mutex::new(None),
            last_bridge_present: Mutex::new(false),
        })
    }

    /// Convenience: a backend whose script ignores its inputs and emits a
    /// fixed sequence of events on the **first** turn (then closes with
    /// `TurnComplete` if the sequence didn't already terminate). Subsequent
    /// turns just emit a bare `TurnComplete` because the events are drained
    /// — `AgentEvent` isn't `Clone` (it carries oneshot senders), so the
    /// batch can't be re-played. Multi-turn tests should use
    /// [`ScriptedBackend::new`] with a closure that produces fresh events
    /// per call.
    pub fn fixed(id: impl Into<String>, mut events_per_turn: Vec<AgentEvent>) -> Arc<Self> {
        Self::new(id, move |_text, _images, tx, _cancel, _bridge| {
            let batch = std::mem::take(&mut events_per_turn);
            tokio::spawn(async move {
                let mut emitted_terminal = false;
                for evt in batch {
                    let terminal = matches!(evt, AgentEvent::TurnComplete | AgentEvent::Error(_));
                    if tx.send(evt).await.is_err() {
                        return;
                    }
                    if terminal {
                        emitted_terminal = true;
                        break;
                    }
                }
                if !emitted_terminal {
                    let _ = tx.send(AgentEvent::TurnComplete).await;
                }
            })
        })
    }

    /// The most recent prompt text the backend received. `None` until the
    /// first turn runs.
    pub fn last_prompt_text(&self) -> Option<String> {
        self.last_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the most recent turn was given a `TextFileBridge` (i.e. the
    /// ACP fs bridge was attached). `false` before any turn runs.
    pub fn last_turn_had_bridge(&self) -> bool {
        *self
            .last_bridge_present
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl SessionBackend for ScriptedBackend {
    async fn run_turn(
        &self,
        text: String,
        images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        text_file_bridge: Option<Arc<dyn TextFileBridge>>,
    ) {
        *self
            .last_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text.clone());
        *self
            .last_bridge_present
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text_file_bridge.is_some();

        // Take the script out, run it (synchronously — it spawns its own
        // task), put it back. Holding the std mutex across the call is
        // fine because the closure body is non-blocking by contract.
        let handle = {
            let mut g = self
                .script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.as_mut()
                .map(|s| s(text, images, event_tx.clone(), cancel, text_file_bridge))
        };
        match handle {
            Some(h) => {
                tokio::spawn(async move {
                    if let Err(e) = h.await {
                        tracing::warn!(error = %e, "scripted turn task ended with error");
                    }
                });
            }
            None => {
                if event_tx
                    .send(AgentEvent::Error("scripted backend exhausted".into()))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        "scripted backend exhausted but receiver dropped before error reached forwarder"
                    );
                }
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.id
    }

    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        // The ACP transport calls `state()` (which routes to GetState ->
        // snapshot_state) immediately after `session/new`/`session/load` to
        // populate `session_info_update`. The default trait impl returns
        // `Err(unsupported)`, which propagates as an internal error and
        // breaks every E2E flow. Return a static default that names the
        // backend so `session_info_update` carries the right session id.
        Ok(SessionStateSnapshot {
            agent_session_id: self.id.clone(),
            title: String::new(),
            message_count: 0,
            user_message_count: 0,
            token_estimate: 0,
            llm_provider: "scripted".into(),
            llm_model: "scripted".into(),
            collab_mode: "execute".into(),
        })
    }
}

/// One-shot factory: hands out a single pre-built [`ScriptedBackend`] on the
/// first `open()` call, then errors. Tests that need multiple sessions push
/// multiple backends in via [`ScriptedFactory::queue`].
pub struct ScriptedFactory {
    backends: Mutex<Vec<Arc<dyn SessionBackend>>>,
    capture_context: Mutex<Vec<Option<ExecutionContext>>>,
}

impl ScriptedFactory {
    /// Single-backend factory.
    pub fn single(backend: Arc<dyn SessionBackend>) -> Arc<Self> {
        Arc::new(Self {
            backends: Mutex::new(vec![backend]),
            capture_context: Mutex::new(Vec::new()),
        })
    }

    /// Multi-backend factory. `open()` consumes them FIFO.
    pub fn queue(backends: Vec<Arc<dyn SessionBackend>>) -> Arc<Self> {
        Arc::new(Self {
            backends: Mutex::new(backends),
            capture_context: Mutex::new(Vec::new()),
        })
    }

    /// All `ExecutionContext`s the daemon passed to `open()`, in call order.
    /// `None` slots reflect calls that opened a session without ACP context.
    /// Tests assert on this to prove the workspace context plumbed through.
    pub fn captured_contexts(&self) -> Vec<Option<ExecutionContext>> {
        self.capture_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl SessionFactory for ScriptedFactory {
    fn open(
        &self,
        _resume_id: Option<&str>,
        execution_context: Option<ExecutionContext>,
    ) -> Result<SessionHost, anyhow::Error> {
        self.capture_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(execution_context);
        let backend = self
            .backends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .ok_or_else(|| anyhow!("ScriptedFactory exhausted"))?;
        Ok(SessionHost::new(backend))
    }
}
