//! Process-wide admission gate for auxiliary (background) LLM work.
//!
//! Every aux-routed client comes from [`super::LlmClient::for_auxiliary`], and
//! every request it makes funnels through `chat`/`chat_streamed`, so this is
//! the single choke point: a new background caller cannot bypass it without
//! deliberately constructing a primary client, which is a different and
//! visible mistake.
//!
//! Why it exists: background work is unattended and bursty. Nightly memory
//! consolidation, title generation, and a scheduled-task fan-out can open
//! dozens of simultaneous requests against the same key, which the provider
//! answers with 429s — self-inflicted throttling that then benches the
//! credential for everyone, including the interactive turn the user is
//! waiting on.
//!
//! Interactive/primary turns are never gated.

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

static GATE: OnceLock<Semaphore> = OnceLock::new();

/// Acquire one aux slot, waiting if the gate is full.
///
/// The limit is fixed on first call for the process lifetime. Resizing a live
/// semaphore would let in-flight callers exceed both the old and the new
/// bound, so a later config change is logged and ignored rather than applied
/// unsoundly — a daemon restart picks it up.
pub(super) async fn acquire(limit: u32) -> SemaphorePermit<'static> {
    let gate = GATE.get_or_init(|| Semaphore::new(limit.max(1) as usize));
    // The semaphore is a process-lifetime static and is never closed, so
    // `acquire` cannot fail.
    #[allow(clippy::expect_used)]
    gate.acquire().await.expect("aux gate is never closed")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    /// The production gate is a process-lifetime static, so these tests drive
    /// a local semaphore through the same acquire/hold/release shape. What is
    /// under test is the bound itself — that N concurrent callers never
    /// overlap more than `limit` at a time.
    async fn peak_overlap(limit: usize, callers: usize) -> usize {
        let gate = Arc::new(Semaphore::new(limit));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..callers {
            let (gate, live, peak) = (gate.clone(), live.clone(), peak.clone());
            handles.push(tokio::spawn(async move {
                let _permit = gate.acquire().await.expect("open");
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("join");
        }
        peak.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn gate_serializes_beyond_limit() {
        // Without a gate this would be 5 — the whole point is that a burst of
        // background work does not become a burst of provider requests.
        assert_eq!(peak_overlap(2, 5).await, 2);
        assert_eq!(peak_overlap(1, 4).await, 1);
    }

    #[tokio::test]
    async fn permit_is_released_when_the_holder_returns_early() {
        // Guards against a future refactor that stashes the permit somewhere
        // it outlives the request — the gate would wedge after `limit` calls.
        let gate = Semaphore::new(1);
        async fn fails(gate: &Semaphore) -> Result<(), &'static str> {
            let _permit = gate.acquire().await.expect("open");
            Err("provider blew up")
        }
        for _ in 0..3 {
            assert!(fails(&gate).await.is_err());
            assert_eq!(
                gate.available_permits(),
                1,
                "an error path must still return its permit"
            );
        }
    }
}
