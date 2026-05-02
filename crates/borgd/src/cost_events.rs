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

    #[test]
    fn disabled_config_starts_without_channel() {
        let bus = CostEventBus::new(&Config::default(), BorgMetrics::noop());
        let rx = bus.subscribe_state();
        assert!(rx.borrow().is_none());
    }

    #[test]
    fn enable_creates_and_disable_drops_channel() {
        let bus = CostEventBus::new(&Config::default(), BorgMetrics::noop());
        bus.set_enabled(true);
        let rx = bus.subscribe_state();
        assert!(rx.borrow().is_some());
        bus.set_enabled(false);
        assert!(rx.borrow().is_none());
    }
}
