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
    /// Create a new queue with the given concurrency limit for total active agent invocations.
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

        // Enforce per-sender session limit: session keys use format "channel:sender:sub"
        if let Some(sender_id) = session_key.split(':').nth(1) {
            let sender_prefix = format!(":{sender_id}:");
            let sender_count = senders
                .keys()
                .filter(|k| k.contains(&sender_prefix))
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

                // Execute all coalesced work items sequentially
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
