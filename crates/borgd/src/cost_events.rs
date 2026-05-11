//! Daemon-wide cost-event bus and runtime toggle handling.
//!
//! This module is intentionally best-effort. Cost emission is optional,
//! consumer-facing, and must never interfere with the main borgd process.

use cortex::config::Config;
use cortex::llm::{CostEvent, CostEventSink};
use cortex::telemetry::BorgMetrics;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, watch};

const COST_EVENT_CAPACITY: usize = 1024;

#[derive(Clone)]
pub(crate) struct InnerBus {
    pub(crate) tx: broadcast::Sender<CostEvent>,
}

/// Optional daemon-wide cost-event broadcaster.
#[derive(Clone)]
pub struct CostEventBus {
    current: Arc<RwLock<Option<Arc<InnerBus>>>>,
    state_tx: watch::Sender<Option<Arc<InnerBus>>>,
    metrics: BorgMetrics,
}

impl CostEventBus {
    /// Construct the manager from the current config.
    pub fn new(config: &Config, metrics: BorgMetrics) -> Self {
        let initial = if config.telemetry.cost_events {
            Some(Arc::new(Self::make_inner()))
        } else {
            None
        };
        let (state_tx, _) = watch::channel(initial.clone());
        Self {
            current: Arc::new(RwLock::new(initial)),
            state_tx,
            metrics,
        }
    }

    fn make_inner() -> InnerBus {
        let (tx, _) = broadcast::channel(COST_EVENT_CAPACITY);
        InnerBus { tx }
    }

    /// Enable or disable the underlying broadcast channel.
    pub fn set_enabled(&self, enabled: bool) {
        let next = {
            let guard = self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match (enabled, guard.as_ref()) {
                (true, Some(existing)) => Some(existing.clone()),
                (true, None) => Some(Arc::new(Self::make_inner())),
                (false, _) => None,
            }
        };

        {
            let mut guard = self
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = next.clone();
        }

        self.state_tx.send_replace(next);
    }

    /// Sync the bus state from the given runtime config.
    pub fn sync_from_config(&self, config: &Config) {
        self.set_enabled(config.telemetry.cost_events);
    }

    /// Subscribe to enable/disable transitions and current sender state.
    pub(crate) fn subscribe_state(&self) -> watch::Receiver<Option<Arc<InnerBus>>> {
        self.state_tx.subscribe()
    }

    /// Increment the lag/drop metric after a subscriber overflow.
    pub fn record_dropped(&self, count: u64) {
        self.metrics.cost_events_dropped.add(count, &[]);
    }
}

impl CostEventSink for CostEventBus {
    fn emit(&self, event: CostEvent) {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(inner) = current {
            let _ = inner.tx.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(request_id: &str) -> CostEvent {
        CostEvent {
            request_id: request_id.to_string(),
            session_id: "sess-1".to_string(),
            provider: "openai".to_string(),
            model: "gpt-test".to_string(),
            cost_usd: Some("0.0042".to_string()),
            cost_usd_present: true,
            cost_source: "provider".to_string(),
            prompt_tokens: 10,
            completion_tokens: 20,
            timestamp_unix_ms: 1_700_000_000_000,
            kind: "chat".to_string(),
        }
    }

    #[test]
    fn disabled_bus_drops_emitted_events() {
        // Config::default() has telemetry.cost_events = false. emit() must
        // be a no-op: no inner channel exists, so a late subscriber cannot
        // observe past events either.
        let bus = CostEventBus::new(&Config::default(), BorgMetrics::noop());
        bus.emit(sample_event("dropped"));

        bus.set_enabled(true);
        let inner = bus
            .subscribe_state()
            .borrow()
            .clone()
            .expect("enabled bus must publish an inner channel");
        let mut rx = inner.tx.subscribe();
        // Channel was just created — no buffered events from before enable.
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "events emitted while disabled must not surface after enable"
        );
    }

    #[tokio::test]
    async fn enabled_bus_broadcasts_events_to_subscribers() {
        let bus = CostEventBus::new(&Config::default(), BorgMetrics::noop());
        bus.set_enabled(true);

        let inner = bus
            .subscribe_state()
            .borrow()
            .clone()
            .expect("enabled bus must publish an inner channel");
        let mut rx = inner.tx.subscribe();

        let event = sample_event("delivered");
        bus.emit(event.clone());

        let received = rx.recv().await.expect("subscriber must receive event");
        assert_eq!(received, event);

        // Disabling tears down the inner channel; future subscribers cannot
        // be created from the dropped sender, and emit() becomes a no-op.
        bus.set_enabled(false);
        assert!(
            bus.subscribe_state().borrow().is_none(),
            "disable must clear the inner channel handle"
        );
        bus.emit(sample_event("after-disable"));
        // The pre-existing receiver may see Closed once the sender is dropped,
        // but it MUST NOT receive the post-disable event.
        match rx.try_recv() {
            Ok(ev) => panic!("post-disable emit must not deliver, got {ev:?}"),
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => {}
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                panic!("unexpected lag on idle channel")
            }
        }
    }
}
