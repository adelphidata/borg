//! Slack slash-command handler.
//!
//! Slack POSTs `application/x-www-form-urlencoded` to a slash-command URL when
//! the user types `/<command> <args>`. The receiver MUST acknowledge within 3
//! seconds (Slack times out and shows an error otherwise). Since agent turns
//! routinely exceed 3 seconds, the flow is:
//!
//! 1. Verify signing secret on the form-encoded body.
//! 2. Parse the form into [`SlashCommandPayload`](super::types::SlashCommandPayload).
//! 3. Return an [`InboundMessage`] for the agent pipeline; the HTTP handler can
//!    immediately respond with `200 OK` empty body.
//! 4. When the agent's reply is ready, [`post_response_url`] sends it back to
//!    Slack via the `response_url` from the original payload.
//!
//! Socket Mode skips the signing-secret check (the WebSocket itself is
//! authenticated) but uses the same parse + response_url path.

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use reqwest::Client;
use serde::Serialize;

use super::types::SlashCommandPayload;
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Visibility of a deferred slash-command reply.
///
/// `InChannel` posts the reply visibly to everyone in the channel; `Ephemeral`
/// shows it only to the invoking user (the Slack default for slash commands).
#[derive(Debug, Clone, Copy)]
pub enum ResponseVisibility {
    /// Visible to the invoking user only.
    Ephemeral,
    /// Visible to all channel members.
    InChannel,
}

impl ResponseVisibility {
    fn as_slack_str(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::InChannel => "in_channel",
        }
    }
}

/// Outcome of handling a slash-command webhook.
pub enum SlashOutcome {
    /// Forward this synthesized message into the agent pipeline. The HTTP
    /// handler should reply `200 OK` with an empty body.
    Forward(Box<InboundMessage>),
    /// The body did not parse — return a 400 to Slack.
    BadRequest(String),
}

/// Handle a slash-command webhook body.
///
/// Verifies the signing secret (when provided), parses the form, and produces
/// an [`InboundMessage`] tagged with `metadata.event_type = "slash_command"`.
/// The original [`SlashCommandPayload`] is embedded in `metadata.payload` so
/// downstream code can extract `response_url` and `trigger_id`.
pub fn handle_slash_command(
    headers: &HeaderMap,
    body: &str,
    signing_secret: Option<&str>,
) -> Result<SlashOutcome> {
    if let Some(secret) = signing_secret {
        super::verify::verify_slack_signature(headers, body, secret)?;
    }

    let payload: SlashCommandPayload = match serde_urlencoded::from_str(body) {
        Ok(p) => p,
        Err(e) => return Ok(SlashOutcome::BadRequest(format!("bad form body: {e}"))),
    };

    Ok(SlashOutcome::Forward(Box::new(payload_to_inbound(payload))))
}

fn payload_to_inbound(p: SlashCommandPayload) -> InboundMessage {
    // The text after the slash command becomes the agent prompt. An empty body
    // (just `/borg`) becomes the command itself, so the agent has *something*
    // to react to instead of running an empty turn.
    let prompt_text = match p.text.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => p.command.clone(),
    };

    let metadata = serde_json::json!({
        "event_type": "slash_command",
        "command": p.command,
        "response_url": p.response_url,
        "trigger_id": p.trigger_id,
        "team_id": p.team_id,
        "channel_name": p.channel_name,
        "user_name": p.user_name,
    });

    InboundMessage {
        sender_id: p.user_id,
        text: prompt_text,
        channel_id: Some(p.channel_id),
        thread_id: None,
        message_id: None,
        thread_ts: None,
        attachments: Vec::new(),
        reaction: None,
        metadata,
        // Slack tags slash-command DMs with `channel_name: "directmessage"`
        // and private groups with `"privategroup"`. Anything else is a
        // public/private channel. Misclassifying flows into DM-vs-group
        // policy gating (gateway.channel_policies, sender pairing), so we
        // surface our best guess and leave None when uncertain.
        peer_kind: match p.channel_name.as_deref() {
            Some("directmessage") => Some(PEER_KIND_DIRECT.to_string()),
            Some(name) if !name.is_empty() => Some(PEER_KIND_GROUP.to_string()),
            _ => None,
        },
    }
}

/// Body sent to a slash-command `response_url` for a deferred reply.
///
/// `response_url` accepts the same shape as `chat.postMessage` for `text`,
/// `blocks`, and `response_type` ("ephemeral" or "in_channel"). The URL is
/// valid for 30 minutes and accepts up to 5 deferred responses.
#[derive(Debug, Serialize)]
pub struct DeferredResponse {
    /// Plain-text fallback (always required, even with blocks).
    pub text: String,
    /// "ephemeral" or "in_channel".
    pub response_type: &'static str,
    /// Optional Block Kit blocks for richer rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<serde_json::Value>>,
    /// If true, replaces the original message (only meaningful inside an
    /// interactive component callback, not a slash command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_original: Option<bool>,
}

/// POST a deferred reply to the `response_url` from a slash-command payload.
///
/// Slack's docs require the body to be a JSON object with `text` and
/// `response_type`. The URL is single-use-ish — repeated POSTs are accepted
/// (up to 5) so the agent can stream multi-part output.
pub async fn post_response_url(
    client: &Client,
    response_url: &str,
    text: impl Into<String>,
    visibility: ResponseVisibility,
    blocks: Option<Vec<serde_json::Value>>,
) -> Result<()> {
    let body = DeferredResponse {
        text: text.into(),
        response_type: visibility.as_slack_str(),
        blocks,
        replace_original: None,
    };

    let resp = client
        .post(response_url)
        .json(&body)
        .send()
        .await
        .context("Failed to POST to slash-command response_url")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("response_url returned {}: {body_text}", status.as_u16());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    fn sign(secret: &str, body: &str) -> HeaderMap {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let basestring = format!("v0:{timestamp}:{body}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(basestring.as_bytes());
        let sig = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("x-slack-signature", HeaderValue::from_str(&sig).unwrap());
        headers
    }

    const VALID_BODY: &str = "command=%2Fborg&text=hello+there&user_id=U123\
        &channel_id=C456&team_id=T1\
        &response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2Fxyz\
        &trigger_id=tr1";

    #[test]
    fn valid_slash_command_forwards_inbound_with_command_text() {
        let outcome = handle_slash_command(&HeaderMap::new(), VALID_BODY, None).unwrap();
        match outcome {
            SlashOutcome::Forward(msg) => {
                assert_eq!(msg.sender_id, "U123");
                assert_eq!(msg.channel_id.as_deref(), Some("C456"));
                // Prompt is the text *after* the slash command, not the command itself.
                assert_eq!(msg.text, "hello there");
                assert_eq!(msg.metadata["event_type"], "slash_command");
                assert_eq!(msg.metadata["command"], "/borg");
                assert_eq!(
                    msg.metadata["response_url"],
                    "https://hooks.slack.com/commands/xyz"
                );
                assert_eq!(msg.metadata["trigger_id"], "tr1");
            }
            SlashOutcome::BadRequest(e) => panic!("expected Forward, got BadRequest: {e}"),
        }
    }

    #[test]
    fn peer_kind_distinguishes_dm_from_channel() {
        // Regression guard: the slash handler MUST NOT hard-code Direct for
        // commands invoked in a public channel. Misclassification cascades
        // into the gateway's DM-vs-group policy gating.
        let dm = "command=%2Fborg&user_id=U1&channel_id=D1&channel_name=directmessage";
        let chan = "command=%2Fborg&user_id=U1&channel_id=C1&channel_name=general";

        let dm_outcome = handle_slash_command(&HeaderMap::new(), dm, None).unwrap();
        let chan_outcome = handle_slash_command(&HeaderMap::new(), chan, None).unwrap();

        let SlashOutcome::Forward(dm_msg) = dm_outcome else {
            panic!("expected Forward for DM");
        };
        let SlashOutcome::Forward(chan_msg) = chan_outcome else {
            panic!("expected Forward for channel");
        };
        assert_eq!(dm_msg.peer_kind.as_deref(), Some("direct"));
        assert_eq!(chan_msg.peer_kind.as_deref(), Some("group"));
    }

    #[test]
    fn empty_text_falls_back_to_command_name() {
        // `/borg` with no args should still produce a meaningful prompt.
        let body = "command=%2Fborg&user_id=U1&channel_id=C1";
        let outcome = handle_slash_command(&HeaderMap::new(), body, None).unwrap();
        match outcome {
            SlashOutcome::Forward(msg) => assert_eq!(msg.text, "/borg"),
            SlashOutcome::BadRequest(_) => panic!("expected Forward"),
        }
    }

    #[test]
    fn malformed_body_returns_bad_request_not_panic() {
        // Missing required field `user_id` — must surface as BadRequest, not unwrap-panic.
        let body = "command=%2Fborg&text=hi";
        let outcome = handle_slash_command(&HeaderMap::new(), body, None).unwrap();
        assert!(matches!(outcome, SlashOutcome::BadRequest(_)));
    }

    #[test]
    fn signing_secret_required_when_provided() {
        let secret = "s3cr3t";
        let headers = sign(secret, VALID_BODY);
        let outcome = handle_slash_command(&headers, VALID_BODY, Some(secret)).unwrap();
        assert!(matches!(outcome, SlashOutcome::Forward(_)));
    }

    #[test]
    fn wrong_signing_secret_rejected() {
        let headers = sign("wrong", VALID_BODY);
        let result = handle_slash_command(&headers, VALID_BODY, Some("correct"));
        assert!(
            result.is_err(),
            "mismatched signature must surface as Err, never silently accept the message"
        );
    }

    #[test]
    fn deferred_response_serializes_with_response_type() {
        let body = DeferredResponse {
            text: "done".into(),
            response_type: "in_channel",
            blocks: None,
            replace_original: None,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["text"], "done");
        assert_eq!(v["response_type"], "in_channel");
        // Optional fields must be omitted, not null — Slack rejects nulls.
        assert!(v.as_object().unwrap().get("blocks").is_none());
        assert!(v.as_object().unwrap().get("replace_original").is_none());
    }

    #[test]
    fn response_visibility_maps_to_slack_strings() {
        assert_eq!(ResponseVisibility::Ephemeral.as_slack_str(), "ephemeral");
        assert_eq!(ResponseVisibility::InChannel.as_slack_str(), "in_channel");
    }
}
