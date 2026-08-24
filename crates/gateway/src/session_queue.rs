use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{info, warn};

/// How long a per-session consumer stays alive without messages before exiting.
const IDLE_TIMEOUT: Duration = Duration::from_secs(cortex::constants::SESSION_IDLE_TIMEOUT_SECS);

/// Maximum buffered messages per session before backpressure kicks in.
const QUEUE_CAPACITY: usize = cortex::constants::SESSION_QUEUE_CAPACITY;

/// Maximum number of concurrent session consumers to prevent memory exhaustion.
const MAX_ACTIVE_SESSIONS: usize = cortex::constants::MAX_ACTIVE_SESSIONS;

/// Maximum sessions a single sender can create across all channels/threads.
const MAX_SESSIONS_PER_SENDER: usize = cortex::constants::MAX_SESSIONS_PER_SENDER;

/// Brief window to coalesce rapid-fire messages from the same session.
const COALESCE_WINDOW: Duration =
    Duration::from_millis(cortex::constants::SESSION_COALESCE_WINDOW_MS);

/// The sender segment of a session key.
///
/// Keys are `channel:sender` or `channel:sender:sub` depending on the channel,
/// so the sender is always the second `:`-delimited field. Returns `None` for
/// a key with no second field.
fn sender_segment(session_key: &str) -> Option<&str> {
    session_key.split(':').nth(1)
}

/// A unit of work to be processed sequentially within a session.
pub type WorkFn = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Per-session sequential work queue.
///
/// Ensures work items targeting the same session key are processed in order,
/// while different sessions can be processed concurrently (up to semaphore limit).
pub struct SessionQueue {
    senders: Arc<Mutex<HashMap<String, mpsc::Sender<WorkFn>>>>,
    semaphore: Arc<Semaphore>,
}

impl SessionQueue {
    /// Build a queue capped at `max_concurrent` simultaneously-executing agent turns.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Enqueue a unit of work for sequential processing within its session.
    /// Spawns a new consumer task if this is the first message for the session.
    /// Returns `false` if the queue is at capacity and the work was dropped.
    pub async fn enqueue(&self, session_key: String, work: WorkFn) -> bool {
        // Fast path: try to send to an existing consumer without holding the lock during send
        let existing_tx = {
            let senders = self.senders.lock().await;
            senders.get(&session_key).cloned()
        };
        if let Some(tx) = existing_tx {
            match tx.send(work).await {
                Ok(()) => return true,
                Err(send_err) => {
                    // Channel closed — consumer exited, remove stale sender and fall through
                    self.senders.lock().await.remove(&session_key);
                    return self.spawn_new_consumer(session_key, send_err.0).await;
                }
            }
        }

        self.spawn_new_consumer(session_key, work).await
    }

    /// Validate capacity limits and spawn a new session consumer.
    async fn spawn_new_consumer(&self, session_key: String, work: WorkFn) -> bool {
        let mut senders = self.senders.lock().await;

        // Re-check: another task may have created a consumer while we waited for the lock
        if let Some(tx) = senders.get(&session_key).cloned() {
            drop(senders);
            match tx.send(work).await {
                Ok(()) => return true,
                Err(send_err) => {
                    // Consumer exited between get and send — clean up and spawn fresh
                    let mut senders = self.senders.lock().await;
                    senders.remove(&session_key);
                    return self.do_spawn(&mut senders, session_key, send_err.0).await;
                }
            }
        }

        self.do_spawn(&mut senders, session_key, work).await
    }

    /// Check capacity limits and spawn a consumer. Caller must hold the lock.
    async fn do_spawn(
        &self,
        senders: &mut HashMap<String, mpsc::Sender<WorkFn>>,
        session_key: String,
        work: WorkFn,
    ) -> bool {
        if senders.len() >= MAX_ACTIVE_SESSIONS {
            warn!(
                "Session queue at capacity ({MAX_ACTIVE_SESSIONS}), dropping work for '{session_key}'"
            );
            return false;
        }

        // Enforce the per-sender session limit by comparing the sender segment
        // of each key. The previous `contains(":{sender}:")` probe required a
        // trailing colon, so it silently never matched two-segment keys
        // (Twilio SMS/WhatsApp emit `channel:sender`) — one SMS sender could
        // occupy the whole pool. Segment equality is also exact, so
        // `+15551234` no longer counts against `+915551234`.
        if let Some(sender_id) = sender_segment(&session_key) {
            let sender_count = senders
                .keys()
                .filter(|k| sender_segment(k) == Some(sender_id))
                .count();
            if sender_count >= MAX_SESSIONS_PER_SENDER {
                warn!(
                    "Sender '{sender_id}' at session limit ({MAX_SESSIONS_PER_SENDER}), dropping work for '{session_key}'"
                );
                return false;
            }
        }

        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let semaphore = self.semaphore.clone();
        let cleanup_senders = self.senders.clone();
        let key = session_key.clone();

        if tx.send(work).await.is_err() {
            warn!("Failed to enqueue first work item for session '{session_key}'");
            return false;
        }

        senders.insert(session_key, tx);

        tokio::spawn(async move {
            session_consumer(&key, rx, semaphore).await;
            cleanup_senders.lock().await.remove(&key);
            info!("Session queue for '{key}' cleaned up after idle timeout");
        });

        true
    }
}

/// Process work items for a single session sequentially.
/// Acquires a semaphore permit before each work item to respect global concurrency limits.
/// Exits after IDLE_TIMEOUT of inactivity.
async fn session_consumer(
    session_key: &str,
    mut rx: mpsc::Receiver<WorkFn>,
    semaphore: Arc<Semaphore>,
) {
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
            Ok(Some(first_work)) => {
                // Coalesce: collect additional work items that arrive within the window.
                // This handles rapid-fire messages (e.g. 10 Slack messages in 1 second).
                let mut batch: Vec<WorkFn> = vec![first_work];
                let deadline = tokio::time::Instant::now() + COALESCE_WINDOW;
                while let Ok(Some(work)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                    batch.push(work);
                }

                // Acquire global concurrency permit
                let _permit = match semaphore.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        warn!("Session queue semaphore closed for '{session_key}'");
                        break;
                    }
                };

                for work in batch {
                    work.await;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    info!("Session consumer for '{session_key}' exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn sender_segment_matches_two_and_three_segment_keys() {
        // Regression: the per-sender cap probed for the substring
        // ":{sender}:", which needs a trailing colon. Twilio SMS/WhatsApp emit
        // the two-segment `channel:sender`, so the probe never matched its own
        // key, `sender_count` was always 0, and MAX_SESSIONS_PER_SENDER was
        // never enforced — one SMS sender could starve the whole pool.
        assert_eq!(sender_segment("sms:+15551234"), Some("+15551234"));
        assert_eq!(sender_segment("slack:U123:C456"), Some("U123"));
        assert_eq!(sender_segment("nocolon"), None);

        // Exact segment equality, so one sender is not counted against another
        // whose id merely contains it.
        assert_ne!(
            sender_segment("sms:+15551234"),
            sender_segment("sms:+915551234")
        );
    }

    #[tokio::test]
    async fn processes_work_sequentially_per_session() {
        let counter = Arc::new(AtomicU32::new(0));

        let queue = SessionQueue::new(10);

        for i in 0..5 {
            let c = counter.clone();
            queue
                .enqueue(
                    "session-1".to_string(),
                    Box::pin(async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        let _ = i;
                    }),
                )
                .await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn different_sessions_get_separate_queues() {
        let counter = Arc::new(AtomicU32::new(0));

        let queue = SessionQueue::new(10);

        for key in &["session-a", "session-b", "session-c"] {
            let c = counter.clone();
            queue
                .enqueue(
                    key.to_string(),
                    Box::pin(async move {
                        c.fetch_add(1, Ordering::SeqCst);
                    }),
                )
                .await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn respects_concurrency_limit() {
        let active = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));

        let queue = SessionQueue::new(2); // Only 2 concurrent

        for i in 0..4 {
            let a = active.clone();
            let m = max_seen.clone();
            queue
                .enqueue(
                    format!("session-{i}"),
                    Box::pin(async move {
                        let current = a.fetch_add(1, Ordering::SeqCst) + 1;
                        m.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        a.fetch_sub(1, Ordering::SeqCst);
                    }),
                )
                .await;
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(max_seen.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn processes_in_strict_fifo_order_per_session() {
        // The count-only existing test passes even if work is reordered.
        // This one pins ordering: every item appends its index to a shared vec
        // and yields, so a non-sequential consumer would interleave indices.
        let observed = Arc::new(Mutex::new(Vec::<u32>::new()));

        let queue = SessionQueue::new(10);

        for i in 0..5u32 {
            let obs = observed.clone();
            queue
                .enqueue(
                    "channel:user:thread".to_string(),
                    Box::pin(async move {
                        // Yield so a concurrent consumer would have a chance to interleave.
                        tokio::task::yield_now().await;
                        obs.lock().await.push(i);
                    }),
                )
                .await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        let final_order = observed.lock().await.clone();
        assert_eq!(final_order, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn enqueue_returns_false_when_per_sender_quota_hit() {
        // Session keys use the format "channel:sender:sub". The per-sender cap
        // looks for the `:sender:` substring in any registered key.
        let queue = SessionQueue::new(10);

        // Block every consumer so the senders map stays at MAX_SESSIONS_PER_SENDER.
        let gate = Arc::new(tokio::sync::Notify::new());

        for i in 0..MAX_SESSIONS_PER_SENDER {
            let g = gate.clone();
            let accepted = queue
                .enqueue(
                    format!("chan:alice:{i}"),
                    Box::pin(async move {
                        g.notified().await;
                    }),
                )
                .await;
            assert!(accepted, "session #{i} for alice must be accepted");
        }

        // Give the spawned consumers a tick to actually register & block on the gate.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Alice's next session must be dropped.
        let dropped = queue
            .enqueue(
                format!("chan:alice:{MAX_SESSIONS_PER_SENDER}"),
                Box::pin(async {}),
            )
            .await;
        assert!(
            !dropped,
            "alice past quota must be rejected, got accepted=true"
        );

        // A different sender is unaffected (proves the cap is per-sender, not global).
        let bob = queue
            .enqueue("chan:bob:0".to_string(), Box::pin(async {}))
            .await;
        assert!(bob, "bob's first session must be accepted");

        // Release the blocked consumers so they can clean up before the test ends.
        for _ in 0..MAX_SESSIONS_PER_SENDER {
            gate.notify_one();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coalesce_window_delays_batch_start_until_window_elapses() {
        // The coalesce deadline is `now + COALESCE_WINDOW`: after the first
        // item arrives the consumer holds it open for the window so rapid
        // follow-ups join the same batch. A deadline in the past would run
        // each item immediately and defeat coalescing entirely.
        let queue = SessionQueue::new(10);
        let ran = Arc::new(AtomicU32::new(0));

        let c = ran.clone();
        queue
            .enqueue(
                "chan:sender".to_string(),
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await;

        // Let the consumer task pick up the item without advancing the clock.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "work must not execute before the coalesce window closes"
        );

        tokio::time::advance(COALESCE_WINDOW + Duration::from_millis(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "work must execute once the coalesce window elapses"
        );
    }

    #[tokio::test]
    async fn coalesces_rapid_messages() {
        let call_count = Arc::new(AtomicU32::new(0));

        let queue = SessionQueue::new(10);

        // Send 5 messages rapidly to the same session — they should be batched
        // into a single coalesce window
        for _ in 0..5 {
            let c = call_count.clone();
            queue
                .enqueue(
                    "session-1".to_string(),
                    Box::pin(async move {
                        c.fetch_add(1, Ordering::SeqCst);
                    }),
                )
                .await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        // All 5 should have been processed (coalescing groups them but still runs each)
        assert_eq!(call_count.load(Ordering::SeqCst), 5);
    }
}
