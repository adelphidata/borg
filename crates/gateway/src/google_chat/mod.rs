pub mod api;
pub mod channel;
pub mod parse;
pub mod types;
pub mod verify;

use anyhow::Result;

use crate::handler::InboundMessage;
use types::ChatEvent;

/// Handle a Google Chat webhook request.
///
/// Flow: parse JSON → verify token → parse event → return.
/// Returns `Ok(None)` for non-message events or bot messages.
pub fn handle_google_chat_webhook(
    body: &str,
    expected_token: Option<&str>,
) -> Result<Option<InboundMessage>> {
    // Parse the event body (single parse)
    let event: ChatEvent = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Invalid Google Chat event JSON: {e}"))?;

    // Verify token on the parsed event
    verify::verify_google_chat_token(&event, expected_token)?;

    // Parse into InboundMessage
    Ok(parse::parse_event(&event))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_body() -> String {
        r#"{
            "type": "MESSAGE",
            "token": "test-token",
            "message": {
                "name": "spaces/SPACE1/messages/MSG1",
                "sender": {
                    "name": "users/USER1",
                    "displayName": "Alice",
                    "type": "HUMAN"
                },
                "text": "@Bot hello",
                "argumentText": "hello",
                "thread": {
                    "name": "spaces/SPACE1/threads/THREAD1"
                }
            },
            "user": {
                "name": "users/USER1",
                "displayName": "Alice",
                "type": "HUMAN"
            },
            "space": {
                "name": "spaces/SPACE1",
                "displayName": "General",
                "type": "ROOM"
            }
        }"#
        .to_string()
    }

    #[test]
    fn full_flow_no_token_fails_closed() {
        let body = message_body();
        let result = handle_google_chat_webhook(&body, None);
        assert!(
            result.is_err(),
            "must fail when no expected token is configured"
        );
        assert!(result.unwrap_err().to_string().contains("not configured"));
    }

    #[test]
    fn full_flow_with_valid_token() {
        let body = message_body();
        let result = handle_google_chat_webhook(&body, Some("test-token")).unwrap();
        let msg = result.unwrap();
        assert_eq!(msg.sender_id, "users/USER1");
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.channel_id.as_deref(), Some("spaces/SPACE1"));
        assert_eq!(
            msg.thread_id.as_deref(),
            Some("spaces/SPACE1/threads/THREAD1")
        );
    }

    #[test]
    fn full_flow_with_invalid_token() {
        let body = message_body();
        let result = handle_google_chat_webhook(&body, Some("wrong-token"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn invalid_json_returns_error() {
        let result = handle_google_chat_webhook("not json", None);
        assert!(result.is_err());
    }

    #[test]
    fn non_message_event_returns_none() {
        let body = r#"{
            "type": "ADDED_TO_SPACE",
            "token": "test-token",
            "space": {"name": "spaces/SPACE1", "type": "DM"},
            "user": {"name": "users/USER1", "type": "HUMAN"}
        }"#;
        let result = handle_google_chat_webhook(body, Some("test-token")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bot_message_returns_none() {
        let body = r#"{
            "type": "MESSAGE",
            "token": "test-token",
            "message": {
                "name": "spaces/SPACE1/messages/MSG1",
                "sender": {
                    "name": "users/BOT1",
                    "displayName": "MyBot",
                    "type": "BOT"
                },
                "text": "I am a bot"
            },
            "space": {"name": "spaces/SPACE1"}
        }"#;
        let result = handle_google_chat_webhook(body, Some("test-token")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn message_without_text_returns_none() {
        let body = r#"{
            "type": "MESSAGE",
            "token": "test-token",
            "message": {
                "name": "spaces/SPACE1/messages/MSG1",
                "sender": {
                    "name": "users/USER1",
                    "type": "HUMAN"
                }
            },
            "space": {"name": "spaces/SPACE1"}
        }"#;
        let result = handle_google_chat_webhook(body, Some("test-token")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn token_verification_before_parse() {
        // Even if the event is a valid message, token mismatch should fail
        let body = message_body();
        let result = handle_google_chat_webhook(&body, Some("bad-token"));
        assert!(result.is_err());
    }
}
