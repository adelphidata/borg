/// Slack Web API HTTP client.
pub mod api;
/// Block Kit builder for outbound messages and modals.
pub mod blocks;
/// Native channel implementation for Slack.
pub mod channel;
/// Event deduplication by event ID.
pub mod dedup;
/// Echo cache for filtering self-sent messages.
pub mod echo;
/// Markdown-to-Slack mrkdwn formatting.
pub mod format;
/// Interactive component handler — buttons, modals, shortcuts, message actions.
pub mod interactive;
/// Slack event parsing into inbound messages.
pub mod parse;
/// Slash command webhook handler and deferred response_url POST helper.
pub mod slash;
/// Socket Mode (WebSocket) transport. Selected automatically when an
/// `xapp-` app-level token is configured.
pub mod socket_mode;
/// Slack API type definitions and envelope parsing.
pub mod types;
/// Typing indicator keepalive for Slack channels.
pub mod typing;
/// Request signature verification using signing secret.
pub mod verify;

use anyhow::Result;
use axum::http::HeaderMap;
use tokio::sync::Mutex;

use crate::handler::InboundMessage;
use dedup::EventDeduplicator;
use echo::EchoCache;
use types::SlackEnvelope;

/// Process an incoming Slack webhook request.
///
/// Flow: check for url_verification → verify signature → dedup → parse event → return.
/// Returns `Ok(SlackWebhookResult::Challenge(..))` for URL verification handshake.
/// Returns `Ok(SlackWebhookResult::Message(..))` for valid inbound messages.
/// Returns `Ok(SlackWebhookResult::Skip)` for bot messages, non-text events, etc.
pub enum SlackWebhookResult {
    /// URL verification challenge — return this string as the HTTP response.
    Challenge(String),
    /// A parsed inbound message ready for agent processing.
    Message(Box<InboundMessage>),
    /// Event was recognized but should be skipped (bot message, non-text, etc.).
    Skip,
}

/// Handle a Slack webhook request.
///
/// If `signing_secret` is provided, the request signature is verified on all requests.
/// If `dedup` is provided, duplicate events are detected and skipped.
/// If `bot_user_id` and `echo_cache` are provided, self-messages are filtered.
pub async fn handle_slack_webhook(
    headers: &HeaderMap,
    body: &str,
    signing_secret: Option<&str>,
    dedup: Option<&Mutex<EventDeduplicator>>,
    bot_user_id: Option<&str>,
    echo_cache: Option<&Mutex<EchoCache>>,
) -> Result<SlackWebhookResult> {
    // Verify signature before processing any payload
    if let Some(secret) = signing_secret {
        verify::verify_slack_signature(headers, body, secret)?;
    }

    let envelope: SlackEnvelope =
        serde_json::from_str(body).map_err(|e| anyhow::anyhow!("Invalid Slack event JSON: {e}"))?;

    match envelope {
        SlackEnvelope::UrlVerification { challenge } => {
            Ok(SlackWebhookResult::Challenge(challenge))
        }
        SlackEnvelope::EventCallback(callback) => {
            if let Some(dedup) = dedup {
                let mut guard = dedup.lock().await;
                // Dedupe by event_id — guards against webhook retries from Slack.
                if let Some(event_id) = callback.event_id.as_deref() {
                    if guard.is_duplicate(event_id) {
                        return Ok(SlackWebhookResult::Skip);
                    }
                }
                // Dedupe by (channel, ts) — guards against Slack delivering the same
                // underlying @mention as both a `message` and an `app_mention` event
                // (two different event_ids for one user action).
                if let (Some(channel), Some(ts)) = (
                    callback.event.channel.as_deref(),
                    callback.event.ts.as_deref(),
                ) {
                    if guard.is_duplicate_channel_ts(channel, ts) {
                        return Ok(SlackWebhookResult::Skip);
                    }
                }
            }

            match parse::parse_event(&callback, bot_user_id, echo_cache).await {
                Some(msg) => Ok(SlackWebhookResult::Message(Box::new(msg))),
                None => Ok(SlackWebhookResult::Skip),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::Arc;

    type HmacSha256 = Hmac<Sha256>;

    fn compute_signature(secret: &str, timestamp: &str, body: &str) -> String {
        let sig_basestring = format!("v0:{timestamp}:{body}");
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(sig_basestring.as_bytes());
        let result = mac.finalize();
        format!("v0={}", hex::encode(result.into_bytes()))
    }

    fn make_signed_headers(secret: &str, body: &str) -> HeaderMap {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let sig = compute_signature(secret, &timestamp, body);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("x-slack-signature", HeaderValue::from_str(&sig).unwrap());
        headers
    }

    #[tokio::test]
    async fn url_verification_returns_challenge() {
        let body = r#"{"type":"url_verification","challenge":"abc123","token":"xyz"}"#;
        let headers = HeaderMap::new();

        let result = handle_slack_webhook(&headers, body, None, None, None, None)
            .await
            .unwrap();
        match result {
            SlackWebhookResult::Challenge(c) => assert_eq!(c, "abc123"),
            _ => panic!("expected Challenge"),
        }
    }

    #[tokio::test]
    async fn url_verification_requires_signature_when_secret_set() {
        let body = r#"{"type":"url_verification","challenge":"abc123","token":"xyz"}"#;
        let secret = "test-signing-secret";
        let headers = make_signed_headers(secret, body);

        let result = handle_slack_webhook(&headers, body, Some(secret), None, None, None)
            .await
            .unwrap();
        match result {
            SlackWebhookResult::Challenge(c) => assert_eq!(c, "abc123"),
            _ => panic!("expected Challenge"),
        }
    }

    #[tokio::test]
    async fn url_verification_fails_with_bad_signature() {
        let body = r#"{"type":"url_verification","challenge":"abc123","token":"xyz"}"#;
        let headers = HeaderMap::new(); // no signature headers

        let result = handle_slack_webhook(&headers, body, Some("secret"), None, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn event_callback_without_secret() {
        let body = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev123",
            "event": {
                "type": "message",
                "user": "U456",
                "text": "hello",
                "ts": "1234567890.123456",
                "channel": "C789"
            }
        }"#;
        let headers = HeaderMap::new();

        let result = handle_slack_webhook(&headers, body, None, None, None, None)
            .await
            .unwrap();
        match result {
            SlackWebhookResult::Message(msg) => {
                assert_eq!(msg.sender_id, "U456");
                assert_eq!(msg.text, "hello");
                assert_eq!(msg.channel_id.as_deref(), Some("C789"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test]
    async fn event_callback_with_valid_signature() {
        let body = r#"{"type":"event_callback","token":"tok","team_id":"T123","event_id":"Ev123","event":{"type":"message","user":"U456","text":"hello","ts":"1234567890.123456","channel":"C789"}}"#;
        let secret = "test-signing-secret";
        let headers = make_signed_headers(secret, body);

        let result = handle_slack_webhook(&headers, body, Some(secret), None, None, None)
            .await
            .unwrap();
        match result {
            SlackWebhookResult::Message(msg) => {
                assert_eq!(msg.text, "hello");
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test]
    async fn event_callback_with_invalid_signature_fails() {
        let body = r#"{"type":"event_callback","token":"tok","team_id":"T123","event_id":"Ev123","event":{"type":"message","user":"U456","text":"hello","ts":"1234567890.123456","channel":"C789"}}"#;
        let headers = make_signed_headers("wrong-secret", body);

        let result =
            handle_slack_webhook(&headers, body, Some("correct-secret"), None, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bot_message_returns_skip() {
        let body = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev123",
            "event": {
                "type": "message",
                "text": "bot says hi",
                "ts": "1234567890.123456",
                "channel": "C789",
                "bot_id": "B123"
            }
        }"#;
        let headers = HeaderMap::new();

        let result = handle_slack_webhook(&headers, body, None, None, None, None)
            .await
            .unwrap();
        assert!(matches!(result, SlackWebhookResult::Skip));
    }

    #[tokio::test]
    async fn invalid_json_returns_error() {
        let headers = HeaderMap::new();
        let result = handle_slack_webhook(&headers, "not json", None, None, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn slack_retry_with_known_event_id_dedups() {
        // Slack retries failed deliveries with X-Slack-Retry-Num set. Dedup
        // (not a header short-circuit) is what suppresses re-processing —
        // critical because Slack only retries when the original failed,
        // i.e. exactly the case where the original may not have been queued.
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-retry-num", HeaderValue::from_static("2"));
        let body = r#"{"type":"event_callback","token":"t","team_id":"T","event_id":"E1","event":{"type":"message","user":"U","text":"hi","ts":"1.1","channel":"C"}}"#;
        let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

        // First delivery: processed normally.
        let first = handle_slack_webhook(&HeaderMap::new(), body, None, Some(&dedup), None, None)
            .await
            .unwrap();
        assert!(matches!(first, SlackWebhookResult::Message(_)));

        // Retry with same event_id: dedups, no double-dispatch.
        let retry = handle_slack_webhook(&headers, body, None, Some(&dedup), None, None)
            .await
            .unwrap();
        assert!(matches!(retry, SlackWebhookResult::Skip));
    }

    #[tokio::test]
    async fn slack_retry_for_unknown_event_id_still_processes() {
        // Regression guard for the previously-buggy header short-circuit:
        // a retry whose original delivery never reached dedup (the original
        // 5xx-d before insert) MUST still parse — otherwise we permanently
        // drop the message Slack is explicitly trying to redeliver.
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-retry-num", HeaderValue::from_static("1"));
        let body = r#"{"type":"event_callback","token":"t","team_id":"T","event_id":"NEW","event":{"type":"message","user":"U","text":"hi","ts":"1.1","channel":"C"}}"#;
        let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

        let result = handle_slack_webhook(&headers, body, None, Some(&dedup), None, None)
            .await
            .unwrap();
        assert!(
            matches!(result, SlackWebhookResult::Message(_)),
            "first-time retry must process, not silently skip"
        );
    }

    #[tokio::test]
    async fn duplicate_event_id_returns_skip() {
        let body = r#"{"type":"event_callback","token":"tok","team_id":"T123","event_id":"Ev123","event":{"type":"message","user":"U456","text":"hello","ts":"1234567890.123456","channel":"C789"}}"#;
        let headers = HeaderMap::new();
        let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

        // First call should parse the message
        let result = handle_slack_webhook(&headers, body, None, Some(&dedup), None, None)
            .await
            .unwrap();
        assert!(matches!(result, SlackWebhookResult::Message(_)));

        // Second call with same event_id should be skipped
        let result = handle_slack_webhook(&headers, body, None, Some(&dedup), None, None)
            .await
            .unwrap();
        assert!(matches!(result, SlackWebhookResult::Skip));
    }
}
