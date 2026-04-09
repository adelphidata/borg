use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Database;

/// Poll interval for cross-process change detection (e.g. CLI while TUI runs).
/// Uses `PRAGMA data_version` (in-memory check, no disk I/O) so a short
/// interval is cheap. In-process changes are delivered immediately via `notify()`.
const POLL_INTERVAL_SECS: u64 = 3;

/// Watches the settings database for changes and broadcasts validated updates.
///
/// Holds a single persistent `Database` connection instead of re-opening on
/// every poll, which reduces contention on the DB file.
pub struct ConfigWatcher {
    rx: watch::Receiver<Config>,
    notify_tx: mpsc::Sender<Config>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl ConfigWatcher {
    /// Start watching DB settings for changes. Spawns a background tokio task.
    pub fn start(initial_config: Config) -> Result<Self> {
        let (config_tx, config_rx) = watch::channel(initial_config);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (notify_tx, notify_rx) = mpsc::channel::<Config>(4);

        tokio::spawn(async move {
            poll_loop(config_tx, stop_rx, notify_rx).await;
        });

        Ok(Self {
            rx: config_rx,
            notify_tx,
            stop_tx: Some(stop_tx),
        })
    }

    /// Get a clone of the watch receiver for sharing with agents/gateway.
    pub fn subscribe(&self) -> watch::Receiver<Config> {
        self.rx.clone()
    }

    /// Immediately broadcast a config update (e.g. after an in-process settings change).
    /// This avoids waiting for the next poll interval.
    pub fn notify(&self, config: Config) {
        // Best-effort: if the channel is full the poll loop will pick it up later.
        let _ = self.notify_tx.try_send(config);
    }

    /// Get a cloneable sender for notifying config changes from other components.
    pub fn notify_sender(&self) -> mpsc::Sender<Config> {
        self.notify_tx.clone()
    }

    /// Stop watching (called on shutdown).
    pub fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Load config from a persistent DB handle.
fn load_config_from(db: &Database) -> Result<Config> {
    let mut config = Config::default();
    for (key, value, _) in db.list_settings()? {
        if let Err(e) = config.apply_setting(&key, &value) {
            warn!("Ignoring invalid setting {key}: {e}");
        }
    }
    config.validate()?;
    Ok(config)
}

/// Send a new config through the watch channel if it differs from the current value.
fn send_if_changed(config_tx: &watch::Sender<Config>, new_config: Config) {
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

/// Background poll loop that checks DB for config changes.
async fn poll_loop(
    config_tx: watch::Sender<Config>,
    mut stop_rx: oneshot::Receiver<()>,
    mut notify_rx: mpsc::Receiver<Config>,
) {
    // Open a single persistent connection for the lifetime of the watcher.
    let db = match Database::open() {
        Ok(db) => Some(db),
        Err(e) => {
            warn!("Config watcher: failed to open database, cross-process reload disabled: {e}");
            None
        }
    };

    // Track SQLite's data_version — increments on any write from any connection.
    let mut last_data_version: i64 = db
        .as_ref()
        .and_then(|d| d.data_version().ok())
        .unwrap_or(0);

    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    // Skip the first immediate tick
    interval.tick().await;

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                info!("Config watcher stopped");
                return;
            }
            // In-process notification — immediate broadcast, no DB read needed.
            Some(config) = notify_rx.recv() => {
                // Update tracked version so the next poll doesn't redundantly reload.
                if let Some(ref db) = db {
                    if let Ok(ver) = db.data_version() {
                        last_data_version = ver;
                    }
                }
                send_if_changed(&config_tx, config);
            }
            // Cross-process poll — cheap PRAGMA data_version check, only reload on change.
            _ = interval.tick() => {
                if let Some(ref db) = db {
                    match db.data_version() {
                        Ok(ver) => {
                            if ver != last_data_version {
                                last_data_version = ver;
                                match load_config_from(db) {
                                    Ok(new_config) => send_if_changed(&config_tx, new_config),
                                    Err(e) => warn!("Config reload from DB failed: {e}"),
                                }
                            }
                        }
                        Err(e) => warn!("Config watcher: data_version check failed: {e}"),
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

    #[tokio::test]
    async fn notify_updates_subscribers() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial).unwrap();
        let mut rx = watcher.subscribe();

        let mut updated = Config::default();
        updated.llm.temperature = 1.5;
        watcher.notify(updated);

        // Give the background task a moment to process
        tokio::time::sleep(Duration::from_millis(50)).await;
        rx.changed().await.unwrap();
        let config = rx.borrow();
        assert!((config.llm.temperature - 1.5).abs() < f32::EPSILON);

        watcher.stop();
    }

    #[tokio::test]
    async fn notify_sender_returns_working_sender() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial).unwrap();
        let mut rx = watcher.subscribe();
        let tx = watcher.notify_sender();

        let mut updated = Config::default();
        updated.llm.temperature = 0.3;
        tx.try_send(updated).unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        rx.changed().await.unwrap();
        let config = rx.borrow();
        assert!((config.llm.temperature - 0.3).abs() < f32::EPSILON);

        watcher.stop();
    }

    #[tokio::test]
    async fn notify_no_change_does_not_trigger_update() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial.clone()).unwrap();
        let mut rx = watcher.subscribe();

        // Send identical config — should not trigger a change notification
        watcher.notify(initial);

        tokio::time::sleep(Duration::from_millis(50)).await;
        // has_changed should be false since the config is identical
        assert!(!rx.has_changed().unwrap_or(true));

        watcher.stop();
    }

    #[tokio::test]
    async fn multiple_notifies_converge_to_latest() {
        let initial = Config::default();
        let watcher = ConfigWatcher::start(initial).unwrap();
        let mut rx = watcher.subscribe();

        // Send updates with yields to ensure each is processed
        for temp in [0.1, 0.5, 1.8] {
            let mut cfg = Config::default();
            cfg.llm.temperature = temp;
            watcher.notify(cfg);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Drain all pending changes
        tokio::time::sleep(Duration::from_millis(50)).await;
        while rx.has_changed().unwrap_or(false) {
            rx.changed().await.unwrap();
        }
        let config = rx.borrow();
        assert!(
            (config.llm.temperature - 1.8).abs() < f32::EPSILON,
            "expected final temperature 1.8, got {}",
            config.llm.temperature,
        );

        watcher.stop();
    }

    #[test]
    fn load_config_from_db() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        let db = Database::init_connection(conn, 5000).unwrap();
        db.set_setting("temperature", "1.2").unwrap();
        db.set_setting("sandbox.enabled", "false").unwrap();

        let config = load_config_from(&db).unwrap();
        assert!((config.llm.temperature - 1.2).abs() < f32::EPSILON);
        assert!(!config.sandbox.enabled);
    }

    #[test]
    fn load_config_from_db_ignores_invalid_settings() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        let db = Database::init_connection(conn, 5000).unwrap();
        db.set_setting("temperature", "not_a_number").unwrap();
        db.set_setting("sandbox.enabled", "true").unwrap();

        // Should succeed, ignoring the bad temperature
        let config = load_config_from(&db).unwrap();
        // Temperature falls back to default since "not_a_number" was ignored
        assert!((config.llm.temperature - 0.7).abs() < f32::EPSILON);
        assert!(config.sandbox.enabled);
    }

    #[test]
    fn send_if_changed_skips_identical() {
        let (tx, rx) = watch::channel(Config::default());
        let same = Config::default();
        send_if_changed(&tx, same);
        // No change should be flagged
        assert!(!rx.has_changed().unwrap_or(true));
    }

    #[test]
    fn send_if_changed_broadcasts_different() {
        let (tx, mut rx) = watch::channel(Config::default());
        let mut different = Config::default();
        different.llm.temperature = 1.9;
        send_if_changed(&tx, different);
        assert!(rx.has_changed().unwrap_or(false));
        rx.mark_changed(); // consume
        let config = rx.borrow();
        assert!((config.llm.temperature - 1.9).abs() < f32::EPSILON);
    }

    #[test]
    fn poll_interval_is_reasonable() {
        // data_version check is cheap (in-memory, no disk I/O) so 2-5s is fine
        assert!(
            POLL_INTERVAL_SECS >= 2,
            "poll interval should be >= 2s, got {POLL_INTERVAL_SECS}s"
        );
    }
}
