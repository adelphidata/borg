use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::api::TelegramClient;
use super::dedup::UpdateDeduplicator;
use super::parse::parse_update;
use super::types::Update;

use cortex::constants;

const POLL_TIMEOUT_SECS: u64 = constants::TELEGRAM_POLL_TIMEOUT_SECS;
const MIN_BACKOFF: Duration = constants::TELEGRAM_MIN_BACKOFF;
const MAX_BACKOFF: Duration = constants::TELEGRAM_MAX_BACKOFF;
const BACKOFF_FACTOR: f64 = constants::TELEGRAM_BACKOFF_FACTOR;
const JITTER_FRACTION: f64 = constants::TELEGRAM_JITTER_FRACTION;
const STALL_TIMEOUT: Duration = constants::TELEGRAM_STALL_TIMEOUT;

/// Callback invoked for each parsed inbound message during polling.
pub type PollCallback = Arc<
    dyn Fn(super::parse::TelegramParsed, i64) -> futures::future::BoxFuture<'static, ()>
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
    // Telegram rejects `getUpdates` while a webhook is registered, so drop any
    // leftover webhook before the first poll.
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

        if consecutive_errors > 0 {
            let backoff = calculate_backoff(consecutive_errors);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
        }

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

                    let chat_id = extract_chat_id(&update);

                    // Parse into a TelegramParsed so the caller can perform
                    // the same media-enrichment flow as the webhook path.
                    if let Some(parsed) = parse_update(&update) {
                        if let Some(cid) = chat_id {
                            callback(parsed, cid).await;
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
    crate::resilience::backoff::calculate_backoff(
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

    mod loop_behavior {
        use super::*;
        use futures::FutureExt;
        use std::time::Instant;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const UPDATES_PATH: &str = "/bot123:ABC/getUpdates";
        const WEBHOOK_PATH: &str = "/bot123:ABC/deleteWebhook";

        fn update_json(update_id: i64, text: &str) -> serde_json::Value {
            serde_json::json!({
                "update_id": update_id,
                "message": {
                    "message_id": 1,
                    "from": { "id": 99, "first_name": "Bob", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "text": text
                }
            })
        }

        async fn mount_delete_webhook(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path(WEBHOOK_PATH))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "ok": true, "result": true })),
                )
                .mount(server)
                .await;
        }

        async fn mount_empty_updates_fallback(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "ok": true, "result": [] })),
                )
                .mount(server)
                .await;
        }

        /// Spawn `run_polling` against the mock server, funneling every
        /// delivered message into a channel as `(text, chat_id)`.
        fn spawn_polling(
            server_uri: &str,
            shutdown: CancellationToken,
        ) -> (
            tokio::sync::mpsc::UnboundedReceiver<(String, i64)>,
            tokio::task::JoinHandle<()>,
        ) {
            let client = Arc::new(
                TelegramClient::with_base("123:ABC", server_uri).expect("client construction"),
            );
            let dedup = Arc::new(Mutex::new(UpdateDeduplicator::new()));
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let callback: PollCallback = Arc::new(move |parsed, chat_id| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send((parsed.inbound.text, chat_id));
                }
                .boxed()
            });
            let handle = tokio::spawn(run_polling(client, dedup, callback, shutdown));
            (rx, handle)
        }

        async fn updates_requests(server: &MockServer) -> Vec<serde_json::Value> {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == UPDATES_PATH)
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect()
        }

        /// Happy path: an update is delivered to the callback, the NEXT
        /// getUpdates request acknowledges it with `offset = update_id + 1`
        /// (the exact value Telegram requires — anything else re-delivers or
        /// skips updates), and a clean iteration polls again immediately
        /// with no idle backoff sleep.
        #[tokio::test]
        async fn delivers_update_and_acknowledges_with_next_offset() {
            let server = MockServer::start().await;
            mount_delete_webhook(&server).await;
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": true, "result": [update_json(5, "hi")] }),
                ))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            mount_empty_updates_fallback(&server).await;

            let shutdown = CancellationToken::new();
            let started = Instant::now();
            let (mut rx, handle) = spawn_polling(&server.uri(), shutdown.clone());

            let (text, chat_id) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("polling must deliver the update")
                .expect("callback channel open");
            assert_eq!((text.as_str(), chat_id), ("hi", 42));
            // MIN_BACKOFF is 2s and jitter only adds; a clean first
            // iteration sleeping the error backoff would land ≥ 2s here.
            assert!(
                started.elapsed() < Duration::from_millis(1900),
                "a clean poll must not sleep the error backoff, took {:?}",
                started.elapsed()
            );

            // Wait for the acknowledging poll after the delivery.
            let deadline = Instant::now() + Duration::from_secs(5);
            while updates_requests(&server).await.len() < 2 {
                assert!(Instant::now() < deadline, "no follow-up getUpdates poll");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            shutdown.cancel();
            handle.await.unwrap();

            let requests = updates_requests(&server).await;
            assert!(
                requests[0].get("offset").is_none(),
                "first poll carries no offset: {}",
                requests[0]
            );
            assert_eq!(
                requests[1]["offset"], 6,
                "processed update 5 must be acknowledged with offset 6"
            );
        }

        /// A transient getUpdates failure must back off (≥ MIN_BACKOFF)
        /// before the retry — a loop that retries hot would hammer the API
        /// during an outage — and then recover and deliver.
        #[tokio::test]
        async fn transient_error_backs_off_before_the_retry() {
            let server = MockServer::start().await;
            mount_delete_webhook(&server).await;
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(ResponseTemplate::new(500).set_body_json(
                    serde_json::json!({ "ok": false, "description": "internal error" }),
                ))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": true, "result": [update_json(7, "back")] }),
                ))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            mount_empty_updates_fallback(&server).await;

            let shutdown = CancellationToken::new();
            let started = Instant::now();
            let (mut rx, handle) = spawn_polling(&server.uri(), shutdown.clone());

            let (text, _) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .expect("polling must recover after a transient error")
                .expect("callback channel open");
            assert_eq!(text, "back");
            assert!(
                started.elapsed() >= Duration::from_millis(1900),
                "retry after an error must wait out MIN_BACKOFF, took {:?}",
                started.elapsed()
            );
            shutdown.cancel();
            handle.await.unwrap();
        }

        /// A 409 Conflict (webhook still registered) must trigger a SECOND
        /// deleteWebhook — beyond the unconditional one at startup — and the
        /// loop must then resume and deliver. Telegram phrases this error as
        /// "Conflict: …" with no literal "409", so detection must accept
        /// either marker alone. The conflict also counts as an error: the
        /// retry must wait out MIN_BACKOFF rather than hammer getUpdates
        /// while the old webhook consumer is still draining.
        #[tokio::test]
        async fn conflict_deletes_webhook_again_and_resumes() {
            let server = MockServer::start().await;
            mount_delete_webhook(&server).await;
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                    "ok": false,
                    "description": "Conflict: terminated by other getUpdates request"
                })))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(UPDATES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": true, "result": [update_json(9, "resumed")] }),
                ))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            mount_empty_updates_fallback(&server).await;

            let shutdown = CancellationToken::new();
            let started = Instant::now();
            let (mut rx, handle) = spawn_polling(&server.uri(), shutdown.clone());

            let (text, _) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .expect("polling must resume after the conflict")
                .expect("callback channel open");
            assert_eq!(text, "resumed");
            assert!(
                started.elapsed() >= Duration::from_millis(1900),
                "retry after a 409 must wait out MIN_BACKOFF, took {:?}",
                started.elapsed()
            );
            shutdown.cancel();
            handle.await.unwrap();

            let webhook_deletes = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == WEBHOOK_PATH)
                .count();
            assert_eq!(
                webhook_deletes, 2,
                "startup delete plus one triggered by the conflict"
            );
        }
    }
}
