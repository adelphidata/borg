/// Telegram Bot API HTTP client.
pub mod api;
/// Native channel implementation for Telegram.
pub mod channel;
/// Per-chat sequential message queue.
pub mod chat_queue;
/// Circuit breaker for Telegram API calls.
pub mod circuit_breaker;
/// Update deduplication by update_id.
pub mod dedup;
/// Markdown-to-Telegram HTML formatting.
pub mod format;
/// Telegram update parsing into inbound messages.
pub mod parse;
/// Long-polling transport for Telegram updates.
pub mod polling;
/// Telegram API type definitions.
pub mod types;
/// Typing indicator keepalive for Telegram chats.
pub mod typing;
/// Webhook secret token verification.
pub mod verify;

use anyhow::Result;
use axum::http::HeaderMap;
use tokio::sync::Mutex;

use dedup::UpdateDeduplicator;
pub use parse::{TelegramAudioRef, TelegramParsed, TelegramPhotoRef};

/// Process an incoming Telegram webhook request.
///
/// Flow: validate body → verify secret → dedup → parse → return.
/// Returns `Ok(None)` for non-text updates or duplicates.
/// The returned [`TelegramParsed`] carries optional media references the
/// caller is responsible for downloading (audio for transcription, photo for
/// vision attachments).
pub async fn handle_telegram_webhook(
    headers: &HeaderMap,
    body: &str,
    secret: Option<&str>,
    dedup: &Mutex<UpdateDeduplicator>,
) -> Result<Option<TelegramParsed>> {
    // Validate and parse the update JSON
    let update = verify::validate_update(body)?;

    // Verify secret token if configured
    if let Some(secret) = secret {
        if !verify::verify_secret_token(headers, secret) {
            anyhow::bail!("Telegram webhook secret verification failed");
        }
    }

    // Deduplicate
    {
        let mut guard = dedup.lock().await;
        if guard.is_duplicate(update.update_id) {
            return Ok(None);
        }
    }

    // Parse into InboundMessage + optional media refs
    Ok(parse::parse_update(&update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn make_headers(secret: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(s) = secret {
            headers.insert(
                "x-telegram-bot-api-secret-token",
                HeaderValue::from_str(s).unwrap(),
            );
        }
        headers
    }

    fn text_body(update_id: i64, text: &str) -> String {
        format!(
            r#"{{
                "update_id": {update_id},
                "message": {{
                    "message_id": 1,
                    "from": {{ "id": 42, "first_name": "Alice", "is_bot": false }},
                    "chat": {{ "id": 42, "type": "private" }},
                    "date": 1700000000,
                    "text": {text_json}
                }}
            }}"#,
            text_json = serde_json::to_string(text).unwrap()
        )
    }

    #[tokio::test]
    async fn full_flow_no_secret() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = text_body(1, "hello");

        let result = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        let msg = result.unwrap().inbound;
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.sender_id, "42");
    }

    #[tokio::test]
    async fn full_flow_with_secret() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = make_headers(Some("secret123"));
        let body = text_body(2, "hi");

        let result = handle_telegram_webhook(&headers, &body, Some("secret123"), &dedup)
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn wrong_secret_rejected() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = make_headers(Some("wrong"));
        let body = text_body(3, "hi");

        let result = handle_telegram_webhook(&headers, &body, Some("secret123"), &dedup).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn duplicate_update_returns_none() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = text_body(10, "hi");

        let first = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        assert!(first.is_some());

        let second = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn non_text_update_returns_none() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        // Message with no text, no media — returns None
        let body = r#"{
            "update_id": 20,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000
            }
        }"#;

        let result = handle_telegram_webhook(&headers, body, None, &dedup)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn photo_message_returns_placeholder() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = r#"{
            "update_id": 21,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }]
            }
        }"#;

        let result = handle_telegram_webhook(&headers, body, None, &dedup)
            .await
            .unwrap();
        let parsed = result.unwrap();
        assert_eq!(parsed.inbound.text, "[Photo]");
        // Photo ref is populated so the channel can download bytes before enqueue.
        let photo = parsed.photo.expect("photo ref populated");
        assert_eq!(photo.file_id, "abc");
    }

    #[tokio::test]
    async fn invalid_body_returns_error() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();

        let result = handle_telegram_webhook(&headers, "not json", None, &dedup).await;
        assert!(result.is_err());
    }
}
