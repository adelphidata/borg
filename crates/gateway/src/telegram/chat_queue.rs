use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use super::parse::{TelegramAudioRef, TelegramInputKind, TelegramParsed, TelegramPhotoRef};

const IDLE_TIMEOUT: Duration = Duration::from_secs(cortex::constants::SESSION_IDLE_TIMEOUT_SECS);
const QUEUE_CAPACITY: usize = cortex::constants::SESSION_QUEUE_CAPACITY;
const MAX_ACTIVE_CHATS: usize = cortex::constants::MAX_ACTIVE_SESSIONS;

/// Telegram's hard per-message length cap, counted in UTF-16 code units (the
/// unit Telegram itself measures in). Clients split anything longer into
/// several updates, which arrive here as separate messages.
const TELEGRAM_MESSAGE_CHAR_LIMIT: usize = 4096;

/// A message at least this long is treated as a possible fragment of a longer
/// paste. Slightly under the hard cap because clients break on a character
/// boundary near the limit rather than exactly at it.
const SPLIT_CHUNK_THRESHOLD: usize = TELEGRAM_MESSAGE_CHAR_LIMIT - 128;

/// How long to wait for the next fragment of a split paste before deciding the
/// message stands alone. Continuation updates follow within milliseconds; a
/// human typing a second message does not.
const CHUNK_COALESCE_WINDOW: Duration = Duration::from_millis(400);

/// Ceilings on a single coalesced paste.
///
/// Every fragment restarts the 400 ms window, so without a cap a sender
/// emitting near-limit messages back to back holds the consumer inside the
/// merge loop indefinitely while the buffer grows without bound — the chat's
/// idle timeout never gets a chance to fire. Past either ceiling the merge
/// stops and the rest arrives as ordinary follow-up messages.
const MAX_COALESCED_FRAGMENTS: usize = 32;
/// Companion to [`MAX_COALESCED_FRAGMENTS`], bounding total merged length.
const MAX_COALESCED_UNITS: usize = MAX_COALESCED_FRAGMENTS * TELEGRAM_MESSAGE_CHAR_LIMIT;

/// Could this text be one fragment of a client-split paste?
///
/// Length alone is the signal, deliberately: the first fragment of a long
/// `/command <prompt>` is a COMMAND update and the rest are plain TEXT
/// updates, so keying on message kind would miss exactly the case that breaks.
/// Short messages — `/stop`, `/approve`, ordinary chat — never qualify, so
/// they keep the immediate path with no added latency.
///
/// Measured in UTF-16 code units, not `char`s: Telegram's cap is a UTF-16
/// length. Counting `char`s would put an emoji-heavy 4096-unit fragment at
/// roughly 2100 chars — under the threshold — so the most visibly split pastes
/// would be the ones the coalescer never noticed.
fn is_probable_split_chunk(text: &str) -> bool {
    text.encode_utf16().count() >= SPLIT_CHUNK_THRESHOLD
}

/// Do two messages belong to the same paste?
///
/// A forum chat multiplexes topics onto one chat queue, and two people can
/// post at once, so fragments must agree on sender and thread before they are
/// joined. Getting this wrong would splice one user's text into another's.
fn same_paste_origin(a: &QueuedMessage, b: &QueuedMessage) -> bool {
    a.thread_id == b.thread_id && a.inbound.sender_id == b.inbound.sender_id
}

/// A message queued for sequential processing within a chat.
pub struct QueuedMessage {
    /// Telegram chat ID this message belongs to.
    pub chat_id: i64,
    /// Parsed inbound message payload.
    pub inbound: crate::handler::InboundMessage,
    /// Audio/voice reference still to be downloaded and transcribed. Media
    /// enrichment runs inside the queue, off the polling loop, so the
    /// reference travels with the message.
    pub audio: Option<TelegramAudioRef>,
    /// Photo reference still to be downloaded into an image attachment.
    pub photo: Option<TelegramPhotoRef>,
    /// What kind of input the update carried (drives voice replies).
    pub input_kind: TelegramInputKind,
    /// Optional thread/topic ID for forum-style chats.
    pub thread_id: Option<i64>,
    /// Optional message ID for reply threading.
    pub message_id: Option<i64>,
}

impl QueuedMessage {
    /// Build the queue entry for one parsed update on `chat_id`.
    pub fn from_parsed(parsed: TelegramParsed, chat_id: i64) -> Self {
        let thread_id = parsed
            .inbound
            .thread_id
            .as_deref()
            .and_then(|id| id.parse().ok());
        let message_id = parsed
            .inbound
            .message_id
            .as_deref()
            .and_then(|id| id.parse().ok());
        Self {
            chat_id,
            inbound: parsed.inbound,
            audio: parsed.audio,
            photo: parsed.photo,
            input_kind: parsed.input_kind,
            thread_id,
            message_id,
        }
    }

    /// Reassemble the parsed update so the media enrichment step can run
    /// on it inside the queue.
    pub fn into_parsed(self) -> (TelegramParsed, i64, Option<i64>, Option<i64>) {
        (
            TelegramParsed {
                inbound: self.inbound,
                audio: self.audio,
                photo: self.photo,
                input_kind: self.input_kind,
            },
            self.chat_id,
            self.thread_id,
            self.message_id,
        )
    }
}

/// A [`PollCallback`](super::polling::PollCallback) that only enqueues: the
/// polling loop returns to `getUpdates` at once and the queue's per-chat
/// consumer runs enrichment, the agent turn and delivery in order. One slow
/// chat no longer stalls every other chat, and a client-split paste is
/// merged before the handler sees it.
pub fn queued_poll_callback(queue: Arc<ChatQueue>) -> super::polling::PollCallback {
    Arc::new(move |parsed, chat_id| {
        let queue = queue.clone();
        Box::pin(async move {
            // `/stop` must not wait behind the turn it is meant to stop: it
            // runs beside the chat's queue, where the handler's cancel path
            // reaches the running turn at once.
            if crate::commands::is_cancel_command(&parsed.inbound.text) {
                let handler = queue.handler.clone();
                tokio::spawn(handler(QueuedMessage::from_parsed(parsed, chat_id)));
                return;
            }
            queue
                .enqueue(QueuedMessage::from_parsed(parsed, chat_id))
                .await;
        })
    })
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

        // Fast path: send to the live consumer with the map lock released.
        // The channel is bounded, so a flooding chat parks *this* send until
        // its consumer catches up; holding the lock across it would park the
        // polling callback and every other chat's enqueue with it.
        let existing_tx = {
            let senders = self.senders.lock().await;
            senders.get(&chat_id).cloned()
        };
        let msg = match existing_tx {
            Some(tx) => match tx.send(msg).await {
                Ok(()) => return,
                Err(e) => {
                    // Channel closed — consumer exited, remove the stale
                    // sender and fall through to spawn a fresh one.
                    self.senders.lock().await.remove(&chat_id);
                    e.0
                }
            },
            None => msg,
        };

        let mut senders = self.senders.lock().await;
        // Another enqueue may have spawned the consumer while the lock was
        // released; hand the message to it rather than racing a second one.
        if let Some(tx) = senders.get(&chat_id).cloned() {
            drop(senders);
            if let Err(e) = tx.send(msg).await {
                warn!(
                    "Chat {chat_id} consumer exited while a message was being enqueued; message dropped: {}",
                    e.0.inbound.text.chars().take(40).collect::<String>()
                );
            }
            return;
        }

        // Reject if at max capacity to prevent memory exhaustion
        if senders.len() >= MAX_ACTIVE_CHATS {
            warn!(
                "Chat queue at capacity ({MAX_ACTIVE_CHATS}), rejecting message for chat {chat_id}"
            );
            return;
        }

        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let handler = self.handler.clone();
        let cleanup_senders = self.senders.clone();

        // Send the first message before spawning the consumer so we surface
        // capacity-exhaustion errors synchronously instead of through a
        // task that's about to exit.
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

/// Absorb the continuation fragments of a client-split paste into `first`.
///
/// Returns the merged message plus, if the wait turned up a message that is
/// *not* part of this paste, that message so the caller can process it next.
/// It is deliberately handed back rather than dropped or re-sent: an mpsc
/// receive cannot be undone, and re-enqueueing would reorder the chat.
async fn coalesce_split_chunks(
    mut first: QueuedMessage,
    rx: &mut mpsc::Receiver<QueuedMessage>,
) -> (QueuedMessage, Option<QueuedMessage>) {
    let mut merged = std::mem::take(&mut first.inbound.text);
    let mut fragments = 1usize;

    loop {
        let Ok(received) = tokio::time::timeout(CHUNK_COALESCE_WINDOW, rx.recv()).await else {
            // Nothing followed in the window — the message stands alone.
            break;
        };
        let Some(next) = received else {
            // Channel closed; deliver what we have.
            break;
        };
        if !same_paste_origin(&first, &next) {
            first.inbound.text = merged;
            return (first, Some(next));
        }

        let more_expected = is_probable_split_chunk(&next.inbound.text);
        merged.push_str(&next.inbound.text);
        fragments += 1;

        // A fragment below the split threshold is the tail of the paste.
        if !more_expected {
            break;
        }

        // Stop merging past either ceiling. Whatever follows is delivered as
        // its own message rather than keeping this loop — and the whole chat
        // queue behind it — running on a stream that never drops below the
        // threshold.
        let units = merged.encode_utf16().count();
        if fragments >= MAX_COALESCED_FRAGMENTS || units >= MAX_COALESCED_UNITS {
            warn!(
                fragments,
                units,
                "Split-paste coalescing hit its ceiling; delivering what has merged so far and \
                 treating the remainder as separate messages"
            );
            break;
        }
    }

    if fragments > 1 {
        info!(
            fragments,
            units = merged.encode_utf16().count(),
            "Coalesced a split Telegram paste into one message"
        );
    }
    first.inbound.text = merged;
    (first, None)
}

/// Process messages for a single chat sequentially.
/// Exits after IDLE_TIMEOUT of inactivity.
///
/// A paste longer than Telegram's per-message cap arrives as several updates.
/// When the first carries a command (`/queue <long prompt>`), dispatching each
/// update independently ran the command against a truncated prompt and then
/// delivered the remaining fragments as fresh messages — which interrupt the
/// turn the command just started. Near-cap messages are therefore held briefly
/// so their continuations can be joined before the handler sees them.
async fn chat_consumer(
    chat_id: i64,
    mut rx: mpsc::Receiver<QueuedMessage>,
    handler: MessageHandler,
) {
    // A message received while coalescing that turned out to belong to a
    // different sender/thread; processed before the channel is polled again.
    let mut carry: Option<QueuedMessage> = None;

    loop {
        let msg = match carry.take() {
            Some(msg) => msg,
            None => match tokio::time::timeout(IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(msg)) => msg,
                // Channel closed, or idle timeout.
                Ok(None) | Err(_) => break,
            },
        };

        let msg = if is_probable_split_chunk(&msg.inbound.text) {
            let (merged, leftover) = coalesce_split_chunks(msg, &mut rx).await;
            carry = leftover;
            merged
        } else {
            msg
        };

        handler(msg).await;
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

    fn queued(text: &str) -> QueuedMessage {
        QueuedMessage {
            chat_id: 1,
            inbound: make_inbound(text),
            audio: None,
            photo: None,
            input_kind: TelegramInputKind::Text,
            thread_id: None,
            message_id: None,
        }
    }

    /// The split threshold sits 128 units under Telegram's 4096-unit cap:
    /// 3968 units and above reads as a probable client-split fragment,
    /// anything shorter keeps the immediate path. The boundary is asserted
    /// with literal lengths so a miscomputed threshold (which would drag
    /// ordinary short messages into the 400 ms coalesce wait) fails here.
    #[test]
    fn split_chunk_threshold_sits_128_under_the_telegram_cap() {
        assert!(
            !is_probable_split_chunk(&"a".repeat(3967)),
            "3967 units is below the threshold"
        );
        assert!(
            is_probable_split_chunk(&"a".repeat(3968)),
            "3968 units (4096 - 128) is a probable fragment"
        );
        // UTF-16 units, not chars: astral-plane symbols count double.
        assert!(
            is_probable_split_chunk(&"𝄞".repeat(1984)),
            "1984 astral chars = 3968 UTF-16 units"
        );
    }

    /// Every near-cap fragment restarts the coalesce window, so a sender that
    /// keeps emitting them holds the consumer in the merge loop and grows the
    /// buffer without bound — the chat's idle timeout can never fire. The merge
    /// has to stop at a ceiling and let the rest arrive as ordinary messages.
    #[tokio::test]
    async fn coalescing_stops_at_the_fragment_ceiling() {
        let (tx, mut rx) = mpsc::channel::<QueuedMessage>(4);
        let chunk = "a".repeat(SPLIT_CHUNK_THRESHOLD);

        // Far more continuation fragments than the ceiling permits, none of
        // them ever dropping below the split threshold.
        let feeder_chunk = chunk.clone();
        let feeder = tokio::spawn(async move {
            for _ in 0..(MAX_COALESCED_FRAGMENTS * 3) {
                if tx.send(queued(&feeder_chunk)).await.is_err() {
                    break;
                }
            }
        });

        let (merged, _leftover) = coalesce_split_chunks(queued(&chunk), &mut rx).await;
        drop(rx);
        let _ = feeder.await;

        let absorbed = merged.inbound.text.len() / chunk.len();
        assert!(
            absorbed <= MAX_COALESCED_FRAGMENTS,
            "merge must stop at the ceiling, absorbed {absorbed} fragments"
        );
        assert!(
            merged.inbound.text.encode_utf16().count() <= MAX_COALESCED_UNITS,
            "merged length must stay under the unit ceiling"
        );
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
                    audio: None,
                    photo: None,
                    input_kind: TelegramInputKind::Text,
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
                    audio: None,
                    photo: None,
                    input_kind: TelegramInputKind::Text,
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
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;
        queue
            .enqueue(QueuedMessage {
                chat_id: 2,
                inbound: make_inbound("fast"),
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
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
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
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
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
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
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;
        queue
            .enqueue(QueuedMessage {
                chat_id: 2,
                inbound: make_inbound("from chat 2"),
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// Collect the text of every message the handler is given, in order.
    fn recording_handler() -> (MessageHandler, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let sink = sink.clone();
            Box::pin(async move {
                sink.lock().await.push(msg.inbound.text);
            })
        });
        (handler, seen)
    }

    fn inbound_from(sender: &str, text: &str) -> crate::handler::InboundMessage {
        let mut inbound = make_inbound(text);
        inbound.sender_id = sender.into();
        inbound
    }

    /// REGRESSION: Telegram splits a paste over 4096 chars into several
    /// updates. A long `/queue <prompt>` therefore arrived as a near-cap
    /// COMMAND chunk plus plain-TEXT continuations — the command ran against a
    /// truncated prompt, and each continuation landed as a fresh message that
    /// interrupted the turn the command had just started. The fragments must
    /// reach the handler as one message.
    #[tokio::test]
    async fn split_paste_is_delivered_as_one_message() {
        let (handler, seen) = recording_handler();
        let queue = ChatQueue::new(handler);

        let head = format!("/queue {}", "a".repeat(SPLIT_CHUNK_THRESHOLD));
        let tail = "b".repeat(50);

        for text in [head.clone(), tail.clone()] {
            queue
                .enqueue(QueuedMessage {
                    chat_id: 1,
                    inbound: make_inbound(&text),
                    audio: None,
                    photo: None,
                    input_kind: TelegramInputKind::Text,
                    thread_id: None,
                    message_id: None,
                })
                .await;
        }

        tokio::time::sleep(CHUNK_COALESCE_WINDOW + Duration::from_millis(300)).await;

        let seen = seen.lock().await;
        assert_eq!(
            seen.len(),
            1,
            "the continuation must not arrive as a second message — that is what \
             interrupts the running turn"
        );
        assert_eq!(
            seen[0],
            format!("{head}{tail}"),
            "the command must see the whole prompt, not a truncated prefix"
        );
    }

    /// Short messages must keep the immediate path. Adding the coalesce delay
    /// to `/stop` would make an interrupt feel broken.
    #[tokio::test]
    async fn short_messages_are_not_delayed_or_merged() {
        let (handler, seen) = recording_handler();
        let queue = ChatQueue::new(handler);

        for text in ["/stop", "/approve"] {
            queue
                .enqueue(QueuedMessage {
                    chat_id: 1,
                    inbound: make_inbound(text),
                    audio: None,
                    photo: None,
                    input_kind: TelegramInputKind::Text,
                    thread_id: None,
                    message_id: None,
                })
                .await;
        }

        // Well under the coalesce window: short messages must already be through.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let seen = seen.lock().await;
        assert_eq!(
            *seen,
            vec!["/stop".to_string(), "/approve".to_string()],
            "short messages must stay separate and immediate"
        );
    }

    /// A forum chat multiplexes topics and senders onto one queue. A message
    /// from someone else that lands mid-coalesce must be delivered on its own,
    /// in order — never spliced into another user's paste, never dropped.
    #[tokio::test]
    async fn foreign_sender_is_not_spliced_into_a_paste() {
        let (handler, seen) = recording_handler();
        let queue = ChatQueue::new(handler);

        let head = "a".repeat(SPLIT_CHUNK_THRESHOLD);
        queue
            .enqueue(QueuedMessage {
                chat_id: 1,
                inbound: inbound_from("42", &head),
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;
        queue
            .enqueue(QueuedMessage {
                chat_id: 1,
                inbound: inbound_from("99", "hello from someone else"),
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(CHUNK_COALESCE_WINDOW + Duration::from_millis(300)).await;

        let seen = seen.lock().await;
        assert_eq!(
            *seen,
            vec![head, "hello from someone else".to_string()],
            "the other sender's message must be delivered separately and in order"
        );
    }

    /// A near-cap message with no continuation is a legitimate long message.
    /// It must still be delivered once the window closes, not held forever.
    #[tokio::test]
    async fn lone_near_cap_message_is_still_delivered() {
        let (handler, seen) = recording_handler();
        let queue = ChatQueue::new(handler);

        let text = "a".repeat(SPLIT_CHUNK_THRESHOLD);
        queue
            .enqueue(QueuedMessage {
                chat_id: 1,
                inbound: make_inbound(&text),
                audio: None,
                photo: None,
                input_kind: TelegramInputKind::Text,
                thread_id: None,
                message_id: None,
            })
            .await;

        tokio::time::sleep(CHUNK_COALESCE_WINDOW + Duration::from_millis(300)).await;

        let seen = seen.lock().await;
        assert_eq!(
            *seen,
            vec![text],
            "a lone long message must not be stranded"
        );
    }

    /// Three-plus fragment pastes must all join: the loop has to keep waiting
    /// while each fragment is itself near the cap.
    #[tokio::test]
    async fn multi_fragment_paste_joins_completely() {
        let (handler, seen) = recording_handler();
        let queue = ChatQueue::new(handler);

        let a = "a".repeat(SPLIT_CHUNK_THRESHOLD);
        let b = "b".repeat(SPLIT_CHUNK_THRESHOLD);
        let c = "c".repeat(10);

        for text in [&a, &b, &c] {
            queue
                .enqueue(QueuedMessage {
                    chat_id: 1,
                    inbound: make_inbound(text),
                    audio: None,
                    photo: None,
                    input_kind: TelegramInputKind::Text,
                    thread_id: None,
                    message_id: None,
                })
                .await;
        }

        tokio::time::sleep(CHUNK_COALESCE_WINDOW * 2 + Duration::from_millis(400)).await;

        let seen = seen.lock().await;
        assert_eq!(seen.len(), 1, "all three fragments belong to one message");
        assert_eq!(seen[0], format!("{a}{b}{c}"));
    }

    fn parsed_from(sender: &str, text: &str) -> TelegramParsed {
        TelegramParsed {
            inbound: inbound_from(sender, text),
            audio: None,
            photo: None,
            input_kind: TelegramInputKind::Text,
        }
    }

    /// The handler that records every chat's completion, with a slow chat 1.
    fn chat_recording_handler(
        seen: Arc<Mutex<Vec<(i64, String)>>>,
        slow_chat: i64,
        delay: Duration,
    ) -> MessageHandler {
        Arc::new(move |msg: QueuedMessage| {
            let seen = seen.clone();
            Box::pin(async move {
                if msg.chat_id == slow_chat {
                    tokio::time::sleep(delay).await;
                }
                seen.lock().await.push((msg.chat_id, msg.inbound.text));
            })
        })
    }

    /// The polling loop must be back at `getUpdates` while a turn runs: the
    /// wired callback returns as soon as the message is queued, and a slow
    /// turn in chat 1 does not delay chat 2's message.
    #[tokio::test]
    async fn queued_callback_returns_at_once_and_isolates_chats() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback = queued_poll_callback(Arc::new(ChatQueue::new(chat_recording_handler(
            seen.clone(),
            1,
            Duration::from_millis(400),
        ))));

        let started = tokio::time::Instant::now();
        callback(parsed_from("42", "slow chat"), 1).await;
        callback(parsed_from("43", "fast chat"), 2).await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "enqueueing must not wait for the handler, took {:?}",
            started.elapsed()
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            *seen.lock().await,
            vec![(2, "fast chat".to_string())],
            "chat 2 must be handled while chat 1's turn is still running"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(seen.lock().await.len(), 2, "chat 1 must still complete");
    }

    /// Messages in one chat keep their arrival order through the queue.
    #[tokio::test]
    async fn queued_callback_keeps_same_chat_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback = queued_poll_callback(Arc::new(ChatQueue::new(chat_recording_handler(
            seen.clone(),
            7,
            Duration::from_millis(20),
        ))));

        for text in ["first", "second", "third"] {
            callback(parsed_from("42", text), 7).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let texts: Vec<String> = seen.lock().await.iter().map(|(_, t)| t.clone()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    /// A client-split paste arriving through the polling callback is merged
    /// before the handler runs — the merge only helps when the queue is
    /// actually in the path.
    #[tokio::test]
    async fn queued_callback_merges_a_split_paste() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback = queued_poll_callback(Arc::new(ChatQueue::new(chat_recording_handler(
            seen.clone(),
            0,
            Duration::ZERO,
        ))));

        let head = "h".repeat(SPLIT_CHUNK_THRESHOLD);
        let tail = "the rest";
        callback(parsed_from("42", &head), 1).await;
        callback(parsed_from("42", tail), 1).await;

        tokio::time::sleep(CHUNK_COALESCE_WINDOW + Duration::from_millis(300)).await;
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 1, "both fragments belong to one message");
        assert_eq!(seen[0].1, format!("{head}{tail}"));
    }

    /// `/stop` runs beside the chat queue, not behind it: with a slow turn
    /// holding the chat, the cancel is handled while that turn still runs.
    #[tokio::test]
    async fn queued_callback_runs_a_cancel_command_without_waiting_for_the_chat() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let seen = seen_c.clone();
            Box::pin(async move {
                if !crate::commands::is_cancel_command(&msg.inbound.text) {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                seen.lock().await.push(msg.inbound.text);
            })
        });
        let callback = queued_poll_callback(Arc::new(ChatQueue::new(handler)));

        callback(parsed_from("42", "long question"), 1).await;
        callback(parsed_from("42", "/stop"), 1).await;

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            *seen.lock().await,
            vec!["/stop".to_string()],
            "the cancel must be handled while the chat's turn is still running"
        );
    }

    /// A photo whose caption was split by the client arrives as a near-cap
    /// fragment plus a continuation. Through the wired callback the handler
    /// must see one message with the full caption, the photo reference
    /// still attached and the thread/reply ids it needs for delivery —
    /// enrichment runs inside the queue, so a reference lost here is a
    /// photo the agent never sees.
    #[tokio::test]
    async fn queued_callback_keeps_media_refs_and_ids_through_the_merge() {
        type Handled = (TelegramParsed, i64, Option<i64>, Option<i64>);
        let seen: Arc<Mutex<Vec<Handled>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let seen = seen_c.clone();
            Box::pin(async move {
                seen.lock().await.push(msg.into_parsed());
            })
        });
        let callback = queued_poll_callback(Arc::new(ChatQueue::new(handler)));

        let head = "c".repeat(SPLIT_CHUNK_THRESHOLD);
        let mut first = parsed_from("42", &head);
        first.inbound.thread_id = Some("12".into());
        first.inbound.message_id = Some("34".into());
        first.photo = Some(TelegramPhotoRef {
            file_id: "f1".into(),
            filename: "photo.jpg".into(),
        });
        first.input_kind = TelegramInputKind::Other;
        let mut second = parsed_from("42", " end of caption");
        second.inbound.thread_id = Some("12".into());

        callback(first, 5).await;
        callback(second, 5).await;
        tokio::time::sleep(CHUNK_COALESCE_WINDOW + Duration::from_millis(300)).await;

        let seen = seen.lock().await;
        assert_eq!(seen.len(), 1, "both fragments belong to one message");
        let (parsed, chat_id, thread_id, message_id) = &seen[0];
        assert_eq!(parsed.inbound.text, format!("{head} end of caption"));
        assert_eq!(
            parsed.photo.as_ref().map(|p| p.file_id.as_str()),
            Some("f1"),
            "the photo reference must reach the handler for enrichment"
        );
        assert_eq!(parsed.input_kind, TelegramInputKind::Other);
        assert_eq!(
            (*chat_id, *thread_id, *message_id),
            (5, Some(12), Some(34)),
            "delivery ids come from the first fragment"
        );
    }

    /// A chat whose bounded queue is full must not stall other chats: the
    /// flooding send parks only its own caller, never the map lock, so a
    /// second chat's enqueue still completes at once.
    #[tokio::test]
    async fn full_chat_queue_does_not_block_another_chats_enqueue() {
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (release_c, seen_c) = (release.clone(), seen.clone());
        let handler: MessageHandler = Arc::new(move |msg: QueuedMessage| {
            let (release, seen) = (release_c.clone(), seen_c.clone());
            Box::pin(async move {
                if msg.chat_id == 1 {
                    // Holds chat 1's consumer so its channel fills up.
                    release.notified().await;
                }
                seen.lock().await.push(msg.chat_id);
            })
        });
        let queue = Arc::new(ChatQueue::new(handler));

        // One in the handler, QUEUE_CAPACITY in the channel, one more that
        // parks in `send` until the consumer drains.
        let flooder = {
            let queue = queue.clone();
            tokio::spawn(async move {
                for _ in 0..(QUEUE_CAPACITY + 2) {
                    queue.enqueue(queued("flood")).await;
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!flooder.is_finished(), "chat 1's last send must be parked");

        let mut other = queued("other chat");
        other.chat_id = 2;
        tokio::time::timeout(Duration::from_millis(200), queue.enqueue(other))
            .await
            .expect("chat 2's enqueue must not wait behind chat 1's full queue");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            *seen.lock().await,
            vec![2],
            "chat 2 is handled while chat 1 is parked"
        );

        release.notify_waiters();
        for _ in 0..(QUEUE_CAPACITY + 2) {
            release.notify_one();
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), flooder).await;
    }
}
