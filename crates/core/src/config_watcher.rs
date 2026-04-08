use std::time::Duration;

use anyhow::Result;
use tokio::sync::{oneshot, watch};
use tracing::{info, warn};

use crate::config::Config;

/// Poll interval for checking DB settings changes.
const POLL_INTERVAL_SECS: u64 = 1;

/// Watches the settings database for changes and broadcasts validated updates.
pub struct ConfigWatcher {
    rx: watch::Receiver<Config>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl ConfigWatcher {
    /// Start watching DB settings for changes. Spawns a background tokio task.
    pub fn start(initial_config: Config) -> Result<Self> {
        let (config_tx, config_rx) = watch::channel(initial_config);
        let (stop_tx, stop_rx) = oneshot::channel();

        tokio::spawn(async move {
            poll_loop(config_tx, stop_rx).await;
        });

        Ok(Self {
            rx: config_rx,
            stop_tx: Some(stop_tx),
        })
    }

    /// Get a clone of the watch receiver for sharing with agents/gateway.
    pub fn subscribe(&self) -> watch::Receiver<Config> {
        self.rx.clone()
    }

    /// Stop watching (called on shutdown).
    pub fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Background poll loop that checks DB for config changes.
async fn poll_loop(config_tx: watch::Sender<Config>, mut stop_rx: oneshot::Receiver<()>) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    // Skip the first immediate tick
    interval.tick().await;

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                info!("Config watcher stopped");
                return;
            }
            _ = interval.tick() => {
                match Config::load_from_db() {
                    Ok(new_config) => {
                        // Only send if config actually changed
                        let _ = config_tx.send_if_modified(|current| {
                            let new_json = serde_json::to_string(&new_config).unwrap_or_default();
                            let cur_json = serde_json::to_string(current).unwrap_or_default();
                            if new_json != cur_json {
                                *current = new_config.clone();
                                true
                            } else {
                                false
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Config reload from DB failed: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_returns_initial_config() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial).unwrap();
        let rx = watcher.subscribe();
        let config = rx.borrow();
        assert!((config.llm.temperature - 0.7).abs() < f32::EPSILON);
        watcher.stop();
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial).unwrap();
        watcher.stop();
    }
}
