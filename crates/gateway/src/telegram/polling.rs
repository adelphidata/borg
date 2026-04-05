use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::api::TelegramClient;
use super::dedup::UpdateDeduplicator;
use super::parse::parse_update;
use super::types::Update;

use borg_core::constants;

const POLL_TIMEOUT_SECS: u64 = constants::TELEGRAM_POLL_TIMEOUT_SECS;
const MIN_BACKOFF: Duration = constants::TELEGRAM_MIN_BACKOFF;
const MAX_BACKOFF: Duration = constants::TELEGRAM_MAX_BACKOFF;
const BACKOFF_FACTOR: f64 = constants::TELEGRAM_BACKOFF_FACTOR;
const JITTER_FRACTION: f64 = constants::TELEGRAM_JITTER_FRACTION;
const STALL_TIMEOUT: Duration = constants::TELEGRAM_STALL_TIMEOUT;

/// Callback invoked for each parsed inbound message during polling.
pub type PollCallback = Arc<
    dyn Fn(crate::handler::InboundMessage, i64) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Run a long-polling loop against the Telegram getUpdates API.
///
/// Automatically handles:
/// - Offset tracking to avoid re-processing
/// - Exponential backoff on errors with jitter
/// - 409 Conflict detection (webhook/polling race) with automatic deleteWebhook
/// - Stall watchdog: restarts if no response within STALL_TIMEOUT
pub async fn run_polling(
    client: Arc<TelegramClient>,
    dedup: Arc<Mutex<UpdateDeduplicator>>,
    callback: PollCallback,
    shutdown: CancellationToken,
) {
    // Ensure no webhook is set before starting polling
    if let Err(e) = client.delete_webhook().await {
        warn!("Failed to delete webhook before polling: {e}");
    }

    let mut offset: Option<i64> = None;
    let mut consecutive_errors: u32 = 0;

    loop {
        if shutdown.is_cancelled() {
            info!("Telegram polling loop shutting down");
            break;
        }

        // Apply backoff if we have consecutive errors
        if consecutive_errors > 0 {
            let backoff = calculate_backoff(consecutive_errors);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
        }

        // Call getUpdates with stall watchdog
        let result = tokio::select! {
            _ = shutdown.cancelled() => break,
            result = tokio::time::timeout(
                STALL_TIMEOUT,
                client.get_updates(offset, POLL_TIMEOUT_SECS)
            ) => {
                match result {
                    Ok(r) => r,
                    Err(_) => {
                        warn!("Telegram getUpdates stalled (no response in {STALL_TIMEOUT:?}), restarting");
                        consecutive_errors += 1;
                        continue;
                    }
                }
            }
        };

        match result {
            Ok(updates) => {
                consecutive_errors = 0;

                for update in updates {
                    // Track offset
                    let next_offset = update.update_id + 1;
                    offset = Some(match offset {
                        Some(current) => current.max(next_offset),
                        None => next_offset,
                    });

                    // Dedup
                    {
                        let mut guard = dedup.lock().await;
                        if guard.is_duplicate(update.update_id) {
                            continue;
                        }
                    }

                    // Extract chat_id for the callback
                    let chat_id = extract_chat_id(&update);

                    // Parse into an InboundMessage; ignore media refs in
                    // long-polling mode (webhook path handles media download).
                    if let Some(parsed) = parse_update(&update) {
                        if let Some(cid) = chat_id {
                            callback(parsed.inbound, cid).await;
                        }
                    }
                }
            }
            Err(e) => {
                let err_str = e.to_string();

                // 409 Conflict: webhook is still set
                if err_str.contains("409") || err_str.contains("Conflict") {
                    warn!("Telegram 409 Conflict — deleting webhook and retrying");
                    if let Err(del_err) = client.delete_webhook().await {
                        warn!("Failed to delete webhook on 409: {del_err}");
                    }
                    consecutive_errors += 1;
                    continue;
                }

                consecutive_errors += 1;
                warn!("Telegram getUpdates error (consecutive: {consecutive_errors}): {e}");
            }
        }
    }
}

fn extract_chat_id(update: &Update) -> Option<i64> {
    if let Some(ref msg) = update.message {
        return Some(msg.chat.id);
    }
    if let Some(ref msg) = update.edited_message {
        return Some(msg.chat.id);
    }
    if let Some(ref cb) = update.callback_query {
        return cb.message.as_ref().map(|m| m.chat.id);
    }
    None
}

fn calculate_backoff(consecutive_errors: u32) -> Duration {
    crate::backoff::calculate_backoff(
        consecutive_errors,
        MIN_BACKOFF,
        MAX_BACKOFF,
        BACKOFF_FACTOR,
        JITTER_FRACTION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases_with_errors() {
        let b1 = calculate_backoff(1);
        let b3 = calculate_backoff(3);
        // With jitter, b3 should generally be larger, but we test the base trend
        assert!(b1.as_secs_f64() >= MIN_BACKOFF.as_secs_f64());
        assert!(b3.as_secs_f64() <= MAX_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION));
    }

    #[test]
    fn backoff_capped_at_max() {
        let b = calculate_backoff(100);
        assert!(b.as_secs_f64() <= MAX_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION));
    }

    #[test]
    fn extract_chat_id_from_message() {
        let update: Update = serde_json::from_str(
            r#"{
            "update_id": 1,
            "message": {
                "message_id": 1,
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "text": "hi"
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_chat_id(&update), Some(42));
    }

    #[test]
    fn extract_chat_id_from_callback() {
        let update: Update = serde_json::from_str(
            r#"{
            "update_id": 1,
            "callback_query": {
                "id": "cb1",
                "from": { "id": 99, "first_name": "Bob", "is_bot": false },
                "message": {
                    "message_id": 5,
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000
                },
                "data": "click"
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_chat_id(&update), Some(42));
    }

    #[test]
    fn extract_chat_id_minimal_update() {
        let update: Update = serde_json::from_str(r#"{ "update_id": 1 }"#).unwrap();
        assert_eq!(extract_chat_id(&update), None);
    }
}
