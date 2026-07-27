//! Panic-isolating respawn wrapper for the gateway's long-running channel
//! loops.
//!
//! The daemon supervises the gateway as one unit: its supervised future is
//! `axum::serve(...)`, which keeps running no matter what happens to the
//! inbound loops spawned alongside it. So a panic in the Telegram poller or
//! the Signal SSE reader was invisible — tokio swallowed it, the join handle
//! was only awaited at shutdown, and the channel went permanently silent
//! while the daemon still reported healthy. This restores the same
//! panic-catch-and-restart discipline the daemon applies to its own
//! subsystems, at the granularity that actually fails.
//!
//! The gateway cannot depend on `borgd` (the dependency runs the other way),
//! so the policy is reimplemented here in miniature: catch panics, log them,
//! restart with capped exponential backoff, and treat a long healthy run as
//! proof the loop recovered.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Delay before the first restart. Doubles per consecutive failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Ceiling on the restart delay — a channel whose upstream is down should
/// keep retrying at a steady, low rate rather than backing off forever.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A run lasting at least this long counts as healthy and resets the backoff,
/// so a loop that crashes once a day is never permanently abandoned.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Spawn `factory`'s future under panic-isolating supervision.
///
/// The factory is called once per (re)spawn — clone any state it needs
/// inside the closure. The returned handle is for the *supervisor*; awaiting
/// it at shutdown drains the supervised loop.
///
/// Restarts are unbounded by design. A channel loop is the only thing
/// standing between the user and a dead integration, and unlike a compute
/// subsystem there is no correctness risk in retrying: the worst case is a
/// steady one-attempt-per-minute reconnect against an upstream that is down.
/// Cancellation via `cancel` is the only clean stop.
pub fn spawn_supervised_loop<F, Fut>(
    name: &'static str,
    cancel: CancellationToken,
    factory: F,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let started = tokio::time::Instant::now();
            let inner = tokio::spawn(factory());
            let abort = inner.abort_handle();
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // The loop should observe the token itself; abort covers
                    // one that is wedged in a call that ignores it.
                    abort.abort();
                    return;
                }
                res = inner => res,
            };

            match outcome {
                Ok(()) => {
                    // A channel loop only returns on shutdown. Returning
                    // while the token is live means it gave up on its own —
                    // restart it rather than leaving the channel dead.
                    if cancel.is_cancelled() {
                        return;
                    }
                    tracing::warn!(
                        loop_name = name,
                        "gateway loop returned while still running; restarting"
                    );
                }
                Err(e) if e.is_cancelled() => return,
                Err(e) => {
                    tracing::error!(
                        loop_name = name,
                        "gateway loop panicked: {e}; restarting in {:?}",
                        backoff
                    );
                }
            }

            if started.elapsed() >= HEALTHY_RUN {
                backoff = INITIAL_BACKOFF;
            }

            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    /// A panicking loop must be restarted, not silently dropped. This is the
    /// whole point of the wrapper: before it, a panic left the channel dead
    /// while the daemon reported healthy.
    #[tokio::test(start_paused = true)]
    async fn panicking_loop_is_restarted() {
        let runs = Arc::new(AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let r = runs.clone();
        let c = cancel.clone();
        let handle = spawn_supervised_loop("test-panic", cancel.clone(), move || {
            let runs = r.clone();
            let cancel = c.clone();
            async move {
                let n = runs.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    panic!("boom {n}");
                }
                // Fourth run behaves: block until shutdown.
                cancel.cancelled().await;
            }
        });

        // Advance past the 1s + 2s + 4s backoffs.
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(
            runs.load(Ordering::SeqCst) >= 4,
            "expected restarts after panics, got {}",
            runs.load(Ordering::SeqCst)
        );

        cancel.cancel();
        handle.await.expect("supervisor exits cleanly");
    }

    /// A loop that returns early while the token is still live has given up
    /// on its own — the supervisor must restart it. (The script poll loops
    /// used to do exactly this and stay dead until a gateway restart.)
    #[tokio::test(start_paused = true)]
    async fn early_return_is_restarted() {
        let runs = Arc::new(AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let r = runs.clone();
        let handle = spawn_supervised_loop("test-return", cancel.clone(), move || {
            let runs = r.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
            }
        });

        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            runs.load(Ordering::SeqCst) > 1,
            "early return must restart, ran {} time(s)",
            runs.load(Ordering::SeqCst)
        );

        cancel.cancel();
        handle.await.expect("supervisor exits cleanly");
    }

    /// Cancellation stops the supervisor without a restart, so shutdown
    /// doesn't spin up a loop that immediately has to be torn down.
    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_without_restart() {
        let runs = Arc::new(AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let r = runs.clone();
        let c = cancel.clone();
        let handle = spawn_supervised_loop("test-cancel", cancel.clone(), move || {
            let runs = r.clone();
            let cancel = c.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                cancel.cancelled().await;
            }
        });

        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();
        handle.await.expect("supervisor exits cleanly");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "cancellation must not trigger a restart"
        );
    }
}
