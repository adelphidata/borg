use anyhow::{bail, Result};

use super::types::ChatEvent;

/// Verify the Google Chat verification token from a parsed event.
///
/// Google Chat can be configured with a verification token that is included
/// in every event payload. This function compares the event's token against
/// the expected value using constant-time comparison.
///
/// If `expected_token` is `None` or empty, verification **fails closed**:
/// unauthenticated webhooks are rejected. Set `GOOGLE_CHAT_WEBHOOK_TOKEN`
/// to enable the channel.
pub fn verify_google_chat_token(event: &ChatEvent, expected_token: Option<&str>) -> Result<()> {
    let expected = match expected_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            bail!(
                "Google Chat webhook verification token not configured — \
                 refusing to accept unverified webhook. Set \
                 GOOGLE_CHAT_WEBHOOK_TOKEN to enable this channel."
            );
        }
    };

    let actual = event
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Google Chat event missing verification token"))?;

    let is_equal = crate::crypto::constant_time_eq(actual.as_bytes(), expected.as_bytes());
    if !is_equal {
        bail!("Google Chat verification token mismatch");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_chat::types::EventType;

    fn make_event(token: Option<&str>) -> ChatEvent {
        ChatEvent {
            event_type: EventType::Message,
            event_time: None,
            token: token.map(String::from),
            message: None,
            user: None,
            space: None,
        }
    }

    #[test]
    fn valid_token_passes() {
        let event = make_event(Some("my-secret-token"));
        assert!(verify_google_chat_token(&event, Some("my-secret-token")).is_ok());
    }

    #[test]
    fn invalid_token_fails() {
        let event = make_event(Some("wrong-token"));
        let result = verify_google_chat_token(&event, Some("my-secret-token"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn no_token_configured_fails_closed() {
        let event = make_event(None);
        let result = verify_google_chat_token(&event, None);
        assert!(result.is_err(), "must reject when token is not configured");
        assert!(result.unwrap_err().to_string().contains("not configured"));
    }

    #[test]
    fn no_token_configured_with_token_in_event_fails_closed() {
        let event = make_event(Some("some-token"));
        let result = verify_google_chat_token(&event, None);
        assert!(result.is_err(), "must reject even if event carries a token");
    }

    #[test]
    fn empty_expected_token_fails_closed() {
        let event = make_event(Some("some-token"));
        let result = verify_google_chat_token(&event, Some(""));
        assert!(
            result.is_err(),
            "empty expected token must be treated as unconfigured"
        );
    }

    #[test]
    fn missing_token_in_event_fails() {
        let event = make_event(None);
        let result = verify_google_chat_token(&event, Some("expected-token"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }
}
