//! Shared typing indicator keepalive loop used by Telegram, Slack, and Discord.
//!
//! Each platform has different keepalive intervals and send functions, but the
//! core loop structure (initial trigger, periodic keepalive, TTL deadline,
//! consecutive failure tracking) is identical.
//!
//! [`TypingIndicatorHandle`] provides the reusable start/stop/Drop pattern so
//! channel modules don't duplicate struct + oneshot + JoinHandle boilerplate.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use borg_core::constants::{TYPING_MAX_CONSECUTIVE_FAILURES, TYPING_MAX_TTL_SECS};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::warn;

/// Platform-specific configuration for the keepalive loop.
pub struct TypingKeepaliveConfig {
    /// How often to re-send the typing action.
    pub keepalive_interval: Duration,
    /// Log prefix for this platform (e.g., "telegram", "slack", "discord").
    pub label: &'static str,
}

/// Run the keepalive loop: initial trigger, then periodic re-sends until
/// stopped, TTL exceeded, or too many consecutive failures.
pub async fn run_keepalive<F, Fut>(
    config: TypingKeepaliveConfig,
    mut stop_rx: oneshot::Receiver<()>,
    send_typing: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), anyhow::Error>>,
{
    let max_ttl = Duration::from_secs(TYPING_MAX_TTL_SECS);

    // Initial typing trigger
    if let Err(e) = send_typing().await {
        warn!("[{} typing] Initial trigger failed: {e}", config.label);
    }

    let mut keepalive_interval = tokio::time::interval(config.keepalive_interval);
    keepalive_interval.tick().await; // consume first immediate tick
    let ttl_deadline = tokio::time::sleep(max_ttl);
    tokio::pin!(ttl_deadline);

    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                break;
            }
            _ = keepalive_interval.tick() => {
                let result = send_typing().await;
                if result.is_err() {
                    consecutive_failures += 1;
                    if consecutive_failures >= TYPING_MAX_CONSECUTIVE_FAILURES {
                        warn!(
                            "[{} typing] {} consecutive failures, stopping keepalive",
                            config.label, TYPING_MAX_CONSECUTIVE_FAILURES
                        );
                        break;
                    }
                } else {
                    consecutive_failures = 0;
                }
            }
            _ = &mut ttl_deadline => {
                warn!(
                    "[{} typing] TTL exceeded ({}s), auto-stopping",
                    config.label, TYPING_MAX_TTL_SECS
                );
                break;
            }
        }
    }
}

/// Reusable handle for a running typing indicator background task.
///
/// Wraps the oneshot-stop + JoinHandle pattern that Discord, Telegram, and
/// Slack all share. Call [`stop`](Self::stop) to gracefully shut down, or let
/// [`Drop`] abort the task if the handle is abandoned.
pub struct TypingIndicatorHandle {
    stop_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl TypingIndicatorHandle {
    /// Spawn a keepalive loop as a background task.
    ///
    /// `spawn_fn` receives a `oneshot::Receiver<()>` (the stop signal) and must
    /// return a future that runs the keepalive loop. This indirection lets each
    /// channel do pre-loop setup (e.g. Slack adds a reaction before entering
    /// `run_keepalive`).
    pub fn start<F>(spawn_fn: F) -> Self
    where
        F: FnOnce(oneshot::Receiver<()>) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + 'static,
    {
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(spawn_fn(stop_rx));
        Self {
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        }
    }

    /// Signal the background task to stop and wait for it to finish.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

/// Ensure the background task is killed if the handle is dropped without `stop()`.
impl Drop for TypingIndicatorHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn test_config(interval_ms: u64) -> TypingKeepaliveConfig {
        TypingKeepaliveConfig {
            keepalive_interval: Duration::from_millis(interval_ms),
            label: "test",
        }
    }

    #[tokio::test]
    async fn test_stop_signal_terminates() {
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(5000);

        let handle = tokio::spawn(async move {
            run_keepalive(config, stop_rx, || async { Ok(()) }).await;
        });

        // Send stop immediately
        stop_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("keepalive did not stop in time")
            .expect("task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn test_ttl_expiration_stops_loop() {
        let (_stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(5000);

        let handle = tokio::spawn(async move {
            run_keepalive(config, stop_rx, || async { Ok(()) }).await;
        });

        // Let spawned task start
        tokio::task::yield_now().await;

        // Advance past TTL (TYPING_MAX_TTL_SECS = 60)
        tokio::time::advance(Duration::from_secs(65)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        // The handle should complete since TTL was exceeded
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("keepalive did not stop after TTL")
            .expect("task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn test_consecutive_failures_exit() {
        let (_stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(1000);

        let handle = tokio::spawn(async move {
            run_keepalive(config, stop_rx, || async {
                Err(anyhow::anyhow!("send failed"))
            })
            .await;
        });

        // Let spawned task start (initial send)
        tokio::task::yield_now().await;

        // Advance past enough ticks for TYPING_MAX_CONSECUTIVE_FAILURES (2)
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(1100)).await;
            tokio::task::yield_now().await;
        }

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("keepalive did not stop after consecutive failures")
            .expect("task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn test_success_resets_failure_counter() {
        let (_stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(1000);
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        // Pattern: fail, succeed (reset), fail, fail (hits 2 consecutive)
        let handle = tokio::spawn(async move {
            run_keepalive(config, stop_rx, move || {
                let n = call_count_clone.fetch_add(1, Ordering::SeqCst);
                async move {
                    // Call 0 = initial (fails, no count)
                    // Call 1 = tick 1 (fail, consecutive=1)
                    // Call 2 = tick 2 (succeed, reset to 0)
                    // Call 3 = tick 3 (fail, consecutive=1)
                    // Call 4 = tick 4 (fail, consecutive=2 => exit)
                    match n {
                        0 | 1 | 3 | 4 => Err(anyhow::anyhow!("fail")),
                        _ => Ok(()),
                    }
                }
            })
            .await;
        });

        tokio::task::yield_now().await;

        // Advance enough for 5+ ticks
        for _ in 0..8 {
            tokio::time::advance(Duration::from_millis(1100)).await;
            tokio::task::yield_now().await;
        }

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("keepalive did not exit")
            .expect("task panicked");

        // With start_paused, execution is deterministic:
        // Call 0=initial(fail), 1=tick(fail,cons=1), 2=tick(succeed,reset),
        // 3=tick(fail,cons=1), 4=tick(fail,cons=2=>exit) = exactly 5 calls
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            5,
            "expected exactly 5 calls (proving reset extended the loop)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_initial_send_failure_continues() {
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(1000);
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        let handle = tokio::spawn(async move {
            run_keepalive(config, stop_rx, move || {
                let n = call_count_clone.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Err(anyhow::anyhow!("initial failure"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await;
        });

        tokio::task::yield_now().await;

        // Advance past a few intervals so subsequent sends happen
        for _ in 0..3 {
            tokio::time::advance(Duration::from_millis(1100)).await;
            tokio::task::yield_now().await;
        }

        // Loop should still be running — verify we got more calls
        assert!(
            call_count.load(Ordering::SeqCst) >= 2,
            "loop should continue after initial failure"
        );

        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("keepalive did not stop")
            .expect("task panicked");
    }

    #[tokio::test(start_paused = true)]
    async fn test_keepalive_sends_periodically() {
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = test_config(1000);
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        tokio::spawn(async move {
            run_keepalive(config, stop_rx, move || {
                call_count_clone.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await;
        });

        tokio::task::yield_now().await;

        // Advance 5 intervals
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(1100)).await;
            tokio::task::yield_now().await;
        }

        // 1 initial + ~5 interval ticks
        let count = call_count.load(Ordering::SeqCst);
        assert!(count >= 3, "expected at least 3 sends, got {count}");

        stop_tx.send(()).unwrap();
    }
}
