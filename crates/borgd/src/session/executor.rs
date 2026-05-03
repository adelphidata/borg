//! Production [`SessionExecutor`] implementation.
//!
//! Looks up or creates a [`SessionHost`] per `session_key`
//! (the gateway computes that as `{sender_id}:{thread_id}`), drives
//! `start_turn` on it, and forwards the host's broadcast events into a fresh
//! `mpsc::Receiver<AgentEvent>` returned to the gateway. This is the
//! replacement for the gateway's old per-message `Agent::new(...)` path —
//! everything routes through the daemon's single-writer-per-session
//! [`SessionRegistry`] now.

use crate::session::{
    backend::{AgentBackend, InlineImage as BackendImage},
    convert, SessionHost, SessionRegistry,
};
use async_trait::async_trait;
use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
use gateway::{InboundTurn, SessionExecutor};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Daemon-side [`SessionExecutor`]. Cheap-to-clone via `Arc` from the gateway
/// caller; holds a [`SessionRegistry`] handle plus the bits needed to mint a
/// fresh [`AgentBackend`] when a new `session_key` shows up.
pub struct DaemonSessionExecutor {
    registry: SessionRegistry,
    config: Config,
    metrics: BorgMetrics,
    config_rx: Option<watch::Receiver<Config>>,
    cost_event_sink: Option<Arc<dyn CostEventSink>>,
}

impl DaemonSessionExecutor {
    /// Build an executor wrapping `registry`. `config` and `metrics` are used
    /// only when a new [`SessionHost`] needs to be constructed; resumed hosts
    /// from the registry already carry their own `Agent`.
    pub fn new(
        registry: SessionRegistry,
        config: Config,
        metrics: BorgMetrics,
        config_rx: Option<watch::Receiver<Config>>,
        cost_event_sink: Option<Arc<dyn CostEventSink>>,
    ) -> Self {
        Self {
            registry,
            config,
            metrics,
            config_rx,
            cost_event_sink,
        }
    }

    fn current_config(&self) -> Config {
        self.config_rx
            .as_ref()
            .map(|rx| rx.borrow().clone())
            .unwrap_or_else(|| self.config.clone())
    }

    fn ensure_host(&self, session_key: &str) -> anyhow::Result<SessionHost> {
        // Today the AgentBackend dispatches all tool calls to daemon-side
        // handlers. The CapabilityRouter is wired and tested in
        // capability_routing.rs but its output is not consumed by the agent
        // loop yet, so terminal.exec / clipboard.read always run on the
        // daemon host even when a client advertises them. Wiring it up is
        // tracked separately and changes routing semantics — leave alone
        // unless updating the routing rules in `protocols::capabilities`.
        if let Some(host) = self.registry.get(session_key) {
            return Ok(host);
        }
        // Resume the per-channel session by passing `session_key` as the
        // resume_id — `Agent::load_session` falls back to a fresh session
        // when the id is unknown, which is the right behavior for the
        // first-ever inbound from this `(sender, thread)`.
        let backend = AgentBackend::new(
            self.current_config(),
            self.metrics.clone(),
            Some(session_key),
            self.config_rx.clone(),
            self.cost_event_sink.clone(),
        )?;
        let host = SessionHost::new(backend);
        self.registry.insert(host.clone());
        Ok(host)
    }
}

#[async_trait]
impl SessionExecutor for DaemonSessionExecutor {
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        let host = self.ensure_host(&turn.session_key)?;

        // Cancel any in-flight turn on this session before starting a new one.
        // Matches today's gateway semantics: a new inbound replaces a stalled
        // reply (the underlying `AgentBackend::run_turn` `try_lock` rejects
        // genuinely-overlapping turns, but cancellation here lets the prior
        // backend turn unblock its mutex before our new turn tries to grab it).
        host.cancel_turn().await;

        let images: Vec<BackendImage> = turn
            .images
            .into_iter()
            .map(|img| BackendImage {
                mime_type: img.mime_type,
                data: img.data,
                filename: img.filename,
            })
            .collect();

        // Subscribe BEFORE start_turn so we can't miss the first event.
        let mut sub = host.subscribe();
        let (cancel, baseline) = host.start_turn(turn.text, images).await;
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);

        // Bridge proto broadcast events back to core AgentEvents for the
        // gateway. Closes when:
        //   - the broadcast channel sees a terminal event (TurnComplete /
        //     Error — including the Error emitted by the backend after
        //     cancel),
        //   - the receiver `rx` is dropped (gateway gave up),
        //   - the broadcast channel itself closes (host dropped).
        //
        // We deliberately do NOT short-circuit on the cancel token here:
        // cancellation flows through the backend, which emits a terminal
        // Error event, which we MUST forward to the gateway so its
        // `collect_agent_response` loop can wind down cleanly. Watching
        // `cancel` ourselves would race against that final event and
        // deliver an empty / partial response.
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(pb) => {
                        if pb.event_seq <= baseline {
                            continue;
                        }
                        let is_terminal = convert::is_terminal(&pb);
                        if let Some(core_evt) = convert::from_proto(pb) {
                            if tx.send(core_evt).await.is_err() {
                                break;
                            }
                        }
                        if is_terminal {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "executor bridge lagged on broadcast channel");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok((rx, cancel))
    }

    async fn cancel(&self, session_key: &str) -> bool {
        if let Some(host) = self.registry.get(session_key) {
            host.cancel_turn().await;
            true
        } else {
            false
        }
    }

    async fn cancel_all(&self) -> usize {
        let ids = self.registry.live_ids();
        let mut count = 0usize;
        for id in &ids {
            if let Some(host) = self.registry.get(id) {
                host.cancel_turn().await;
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_config_prefers_live_watcher_value_for_new_sessions() {
        let mut startup = Config::default();
        startup.llm.model = "startup-model".to_string();

        let mut live = startup.clone();
        live.llm.model = "live-model".to_string();

        let (tx, rx) = watch::channel(startup.clone());
        tx.send_replace(live.clone());

        // Simulate an operator update that happened after executor startup.
        // New hosts must see the watcher's current value immediately rather
        // than waiting for a later has_changed() edge inside Agent.
        let executor = DaemonSessionExecutor::new(
            SessionRegistry::new(),
            startup,
            BorgMetrics::noop(),
            Some(rx),
            None,
        );

        assert_eq!(executor.current_config().llm.model, live.llm.model);
    }
}
