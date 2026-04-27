use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

const IDLE_TIMEOUT: Duration = Duration::from_secs(cortex::constants::SESSION_IDLE_TIMEOUT_SECS);
const QUEUE_CAPACITY: usize = cortex::constants::SESSION_QUEUE_CAPACITY;
const MAX_ACTIVE_CHATS: usize = cortex::constants::MAX_ACTIVE_SESSIONS;

/// A message queued for sequential processing within a chat.
pub struct QueuedMessage {
    /// Telegram chat ID this message belongs to.
    pub chat_id: i64,
    /// Parsed inbound message payload.
    pub inbound: crate::handler::InboundMessage,
    /// Optional thread/topic ID for forum-style chats.
    pub thread_id: Option<i64>,
    /// Optional message ID for reply threading.
    pub message_id: Option<i64>,
}

/// Handler function type for processing queued messages.
pub type MessageHandler =
    Arc<dyn Fn(QueuedMessage) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Per-chat sequential message queue.
///
/// Ensures messages within the same chat are processed in order,
/// while different chats can be processed concurrently.
pub struct ChatQueue {
    senders: Arc<Mutex<HashMap<i64, mpsc::Sender<QueuedMessage>>>>,
    handler: MessageHandler,
}

impl ChatQueue {
    /// Create a new chat queue with the given message handler.
    pub fn new(handler: MessageHandler) -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
            handler,
        }
    }

    /// Enqueue a message for sequential processing in its chat.
    /// Spawns a new consumer task if this is the first message for the chat.
    pub async fn enqueue(&self, msg: QueuedMessage) {
        let chat_id = msg.chat_id;
        let mut senders = self.senders.lock().await;

        // Try to send to existing queue
        let msg = if let Some(tx) = senders.get(&chat_id) {
            match tx.send(msg).await {
                Ok(()) => return,
                Err(e) => {
                    // Channel closed — consumer exited, remove stale sender
                    senders.remove(&chat_id);
                    e.0
                }
            }
        } else {
            msg
        };

        // Reject if at max capacity to prevent memory exhaustion
        if senders.len() >= MAX_ACTIVE_CHATS {
            warn!(
                "Chat queue at capacity ({MAX_ACTIVE_CHATS}), rejecting message for chat {chat_id}"
            );
            return;
        }

        // Spawn a new consumer for this chat
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let handler = self.handler.clone();
        let cleanup_senders = self.senders.clone();

        // Send the first message before spawning the consumer
        if tx.send(msg).await.is_err() {
            warn!("Failed to enqueue first message for chat {chat_id}");
            return;
        }

        senders.insert(chat_id, tx);

        tokio::spawn(async move {
            chat_consumer(chat_id, rx, handler).await;

            // Clean up when consumer exits
            let mut senders = cleanup_senders.lock().await;
            senders.remove(&chat_id);
            info!("Chat queue for {chat_id} cleaned up after idle timeout");
        });
    }
}

/// Process messages for a single chat sequentially.
/// Exits after IDLE_TIMEOUT of inactivity.
async fn chat_consumer(
    chat_id: i64,
    mut rx: mpsc::Receiver<QueuedMessage>,
    handler: MessageHandler,
) {
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
            Ok(Some(msg)) => {
                handler(msg).await;
            }
            Ok(None) => {
                // Channel closed
                break;
            }
            Err(_) => {
                // Idle timeout
                break;
            }
        }
    }
    info!("Chat consumer for {chat_id} exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn make_inbound(text: &str) -> crate::handler::InboundMessage {
        crate::handler::InboundMessage {
            sender_id: "42".into(),
            text: text.into(),
            channel_id: Some("42".into()),
            thread_id: None,
            message_id: None,
            thread_ts: None,
            attachments: Vec::new(),
            reaction: None,
            metadata: serde_json::Value::Null,
            peer_kind: None,
        }
    }

    #[tokio::test]
    async fn processes_messages_sequentially() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let handler: MessageHandler = Arc::new(move |_msg: QueuedMessage| {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });

        let queue = ChatQueue::new(handler);

        for i in 0..5 {
            queue
                .enqueue(QueuedMessage {
                    chat_id: 1,
                    inbound: make_inbound(&format!("msg {i}")),
                    thread_id: None,
                    message_id: None,
                })
                .await;
        }

        // Give the consumer time to process
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn queue_preserves_message_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = order.clone();

        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let o = order_clone.clone();
            Box::pin(async move {
                o.lock().await.push(msg.inbound.text.clone());
            })
        });

        let queue = ChatQueue::new(handler);

        for i in 0..10 {
            queue
                .enqueue(QueuedMessage {
                    chat_id: 1,
                    inbound: make_inbound(&format!("msg-{i}")),
                    thread_id: None,
                    message_id: None,
                })
                .await;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
        let recorded = order.lock().await;
        let expected: Vec<String> = (0..10).map(|i| format!("msg-{i}")).collect();
        assert_eq!(*recorded, expected, "messages must be processed in order");
    }

    #[tokio::test]
    async fn concurrent_chats_isolated() {
        let chat1_done = Arc::new(tokio::sync::Notify::new());
        let chat2_done = Arc::new(tokio::sync::Notify::new());
        let chat1_done_c = chat1_done.clone();
        let chat2_done_c = chat2_done.clone();

        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let c1 = chat1_done_c.clone();
            let c2 = chat2_done_c.clone();
            Box::pin(async move {
                if msg.chat_id == 1 {
                    // Slow handler
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    c1.notify_one();
                } else {
                    // Fast handler
                    c2.notify_one();
                }
            })
        });

        let queue = ChatQueue::new(handler);

        queue
            .enqueue(QueuedMessage {
                chat_id: 1,
                inbound: make_inbound("slow"),
                thread_id: None,
                message_id: None,
            })
            .await;
        queue
            .enqueue(QueuedMessage {
                chat_id: 2,
                inbound: make_inbound("fast"),
                thread_id: None,
                message_id: None,
            })
            .await;

        // Chat 2 should complete before chat 1
        tokio::time::timeout(Duration::from_millis(100), chat2_done.notified())
            .await
            .expect("fast chat should complete quickly");
    }

    #[tokio::test]
    async fn re_enqueue_to_active_sender() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let handler: MessageHandler = Arc::new(move |_msg: QueuedMessage| {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });

        let queue = ChatQueue::new(handler);

        // First message — spawns a consumer
        queue
            .enqueue(QueuedMessage {
                chat_id: 99,
                inbound: make_inbound("first"),
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Wait for idle timeout so the consumer exits and sender becomes stale.
        // IDLE_TIMEOUT is SESSION_IDLE_TIMEOUT_SECS (30s by default), but we can't
        // wait that long. Instead, test that re-enqueue after a short wait still works
        // (the fast path with active sender).
        queue
            .enqueue(QueuedMessage {
                chat_id: 99,
                inbound: make_inbound("second"),
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "second message should be processed via existing or respawned consumer"
        );
    }

    #[tokio::test]
    async fn different_chats_get_separate_queues() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let handler: MessageHandler = Arc::new(move |_msg: QueuedMessage| {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });

        let queue = ChatQueue::new(handler);

        queue
            .enqueue(QueuedMessage {
                chat_id: 1,
                inbound: make_inbound("from chat 1"),
                thread_id: None,
                message_id: None,
            })
            .await;
        queue
            .enqueue(QueuedMessage {
                chat_id: 2,
                inbound: make_inbound("from chat 2"),
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
