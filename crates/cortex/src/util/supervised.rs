//! Panic-visibility wrapper for detached long-running tasks.
//!
//! `tokio::spawn` swallows panics: a detached task that panics simply
//! disappears, leaving the parent process running but with one fewer
//! worker. For long-running internal loops (delivery drainers, polling
//! consumers, per-channel listeners) that's invisible to operators —
//! the symptom is "queues stop draining" with no log line.
//!
//! `spawn_panic_logged` wraps the future in `catch_unwind` and emits a
//! `tracing::error!` on panic so the failure shows up in `daemon.err`
//! and the JSONL log. It does NOT respawn — outer supervision (e.g.
//! `borgd::supervisor::spawn_supervised` wrapping the gateway as a
//! whole) handles process-level restart. This helper is *visibility
//! only*: turn silent disappearance into a loud, debuggable event.

use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use crate::util::panic_utils::panic_msg;

/// Spawn `fut` on the current Tokio runtime and log any panic with `name`
/// as the identifying label. The returned `JoinHandle` always resolves to
/// `()` — the panic is consumed by `catch_unwind`.
pub fn spawn_panic_logged<F>(name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(payload) = AssertUnwindSafe(fut).catch_unwind().await {
            tracing::error!(
                task = name,
                "long-running task `{name}` panicked: {}",
                panic_msg(&payload)
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn panicking_task_is_caught_and_join_completes_cleanly() {
        // The wrapper must not propagate the panic to its JoinHandle —
        // otherwise the caller would still see a JoinError and the
        // visibility benefit would be lost. We verify by joining and
        // asserting the join itself succeeds.
        let handle = spawn_panic_logged("test-panic", async {
            panic!("boom from test");
        });
        let join_result = handle.await;
        assert!(
            join_result.is_ok(),
            "spawn_panic_logged should consume panics, not surface them via JoinError"
        );
    }

    #[tokio::test]
    async fn non_panicking_task_runs_to_completion() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let handle = spawn_panic_logged("test-ok", async move {
            ran_clone.store(true, Ordering::SeqCst);
        });
        handle.await.expect("clean future should join Ok");
        assert!(ran.load(Ordering::SeqCst), "future body must execute");
    }
}
