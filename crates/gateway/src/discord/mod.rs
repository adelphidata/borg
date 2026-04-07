/// Discord Bot API HTTP client.
pub mod api;
/// Native channel implementation for Discord.
pub mod channel;
/// Bounded deduplicator for Discord interaction IDs.
pub mod dedup;
/// Markdown formatting for Discord messages.
pub mod format;
/// Discord interaction parsing into inbound messages.
pub mod parse;
/// Discord API type definitions.
pub mod types;
/// Typing indicator keepalive for Discord channels.
pub mod typing;
/// Ed25519 signature verification for interaction webhooks.
pub mod verify;

use anyhow::Result;
use axum::http::HeaderMap;

use crate::handler::InboundMessage;
use types::{Interaction, InteractionResponse, InteractionType};

/// Result of processing a Discord interaction webhook.
pub enum DiscordWebhookResult {
    /// Ping interaction — return the pong response as the HTTP body.
    Pong(InteractionResponse),
    /// A parsed inbound message with the original interaction (needed for responding).
    Message(InboundMessage, Box<Interaction>),
    /// Interaction came from a guild not in the configured allowlist — reject silently.
    GuildNotAllowed {
        /// The guild ID that was rejected (for logging / observability).
        guild_id: String,
    },
    /// Interaction was recognized but should be skipped (bot, autocomplete, etc.).
    Skip,
}

/// Handle a Discord interaction webhook request.
///
/// If `public_key_hex` is provided, the request signature is verified using Ed25519.
/// If `guild_allowlist` is provided and non-empty, interactions from guilds not in
/// the list are rejected with `GuildNotAllowed`. DM interactions (no `guild_id`) are
/// unaffected by the guild allowlist — DM access is gated separately by `dm_policy`
/// in `handler::invoke_agent`.
///
/// Returns `Pong` for ping interactions, `Message` for actionable interactions,
/// `GuildNotAllowed` for rejected guilds, and `Skip` for everything else (bots,
/// autocomplete, unknown types).
pub fn handle_discord_webhook(
    headers: &HeaderMap,
    body: &str,
    public_key_hex: Option<&str>,
    guild_allowlist: Option<&[String]>,
) -> Result<DiscordWebhookResult> {
    // Verify signature if public key is configured
    if let Some(key) = public_key_hex {
        verify::verify_discord_signature(headers, body, key)?;
    }

    let interaction: Interaction = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Invalid Discord interaction JSON: {e}"))?;

    // Handle Ping → Pong (allowlist does not apply to pings — they come from Discord)
    if interaction.interaction_type == InteractionType::Ping {
        return Ok(DiscordWebhookResult::Pong(InteractionResponse::pong()));
    }

    // Enforce guild allowlist. An empty/missing list means "allow all". DMs (no
    // guild_id) bypass this check — DM access control lives in dm_policy.
    if let Some(allowlist) = guild_allowlist {
        if !allowlist.is_empty() {
            if let Some(guild_id) = &interaction.guild_id {
                if !allowlist.iter().any(|g| g == guild_id) {
                    return Ok(DiscordWebhookResult::GuildNotAllowed {
                        guild_id: guild_id.clone(),
                    });
                }
            }
        }
    }

    // Try to parse into an InboundMessage
    match parse::parse_interaction(&interaction) {
        Some(msg) => Ok(DiscordWebhookResult::Message(msg, Box::new(interaction))),
        None => Ok(DiscordWebhookResult::Skip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_signed_headers(signing_key: &SigningKey, body: &str) -> (HeaderMap, String) {
        let timestamp = "1234567890";
        let message = format!("{timestamp}{body}");
        let signature = signing_key.sign(message.as_bytes());
        let sig_hex = hex::encode(signature.to_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-signature-ed25519",
            HeaderValue::from_str(&sig_hex).unwrap(),
        );
        headers.insert(
            "x-signature-timestamp",
            HeaderValue::from_str(timestamp).unwrap(),
        );

        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        (headers, public_key_hex)
    }

    #[test]
    fn ping_returns_pong() {
        let body = r#"{"id":"1","type":1,"token":"tok"}"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        match result {
            DiscordWebhookResult::Pong(resp) => {
                assert_eq!(resp.response_type, 1);
            }
            _ => panic!("expected Pong"),
        }
    }

    #[test]
    fn ping_with_valid_signature() {
        let body = r#"{"id":"1","type":1,"token":"tok"}"#;
        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let (headers, pub_hex) = make_signed_headers(&signing_key, body);

        let result = handle_discord_webhook(&headers, body, Some(&pub_hex), None).unwrap();
        match result {
            DiscordWebhookResult::Pong(resp) => {
                assert_eq!(resp.response_type, 1);
            }
            _ => panic!("expected Pong"),
        }
    }

    #[test]
    fn slash_command_returns_message() {
        let body = r#"{
            "id": "2",
            "type": 2,
            "token": "tok",
            "data": { "id": "cmd1", "name": "ask", "options": [{"name":"q","value":"hello"}] },
            "member": { "user": { "id": "u1", "username": "alice" } },
            "channel_id": "ch1"
        }"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        match result {
            DiscordWebhookResult::Message(msg, interaction) => {
                assert_eq!(msg.sender_id, "u1");
                assert_eq!(msg.text, "hello");
                assert_eq!(msg.channel_id.as_deref(), Some("ch1"));
                assert_eq!(interaction.token, "tok");
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn invalid_json_returns_error() {
        let headers = HeaderMap::new();
        let result = handle_discord_webhook(&headers, "not json", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_signature_fails() {
        let body = r#"{"id":"1","type":1,"token":"tok"}"#;
        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let wrong_key = SigningKey::from_bytes(&[4u8; 32]);
        let (headers, _) = make_signed_headers(&signing_key, body);
        let wrong_pub_hex = hex::encode(wrong_key.verifying_key().to_bytes());

        let result = handle_discord_webhook(&headers, body, Some(&wrong_pub_hex), None);
        assert!(result.is_err());
    }

    fn slash_command_body(guild_id: Option<&str>) -> String {
        let guild_field = match guild_id {
            Some(id) => format!(r#","guild_id":"{id}""#),
            None => String::new(),
        };
        format!(
            r#"{{
                "id": "int42",
                "type": 2,
                "token": "tok",
                "data": {{ "id": "cmd1", "name": "ask", "options": [{{"name":"q","value":"hi"}}] }},
                "member": {{ "user": {{ "id": "u1", "username": "alice" }} }},
                "channel_id": "ch1"{guild_field}
            }}"#
        )
    }

    #[test]
    fn allowlist_none_allows_any_guild() {
        let body = slash_command_body(Some("guild_a"));
        let headers = HeaderMap::new();
        let result = handle_discord_webhook(&headers, &body, None, None).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Message(_, _)));
    }

    #[test]
    fn allowlist_empty_allows_any_guild() {
        let body = slash_command_body(Some("guild_a"));
        let headers = HeaderMap::new();
        let empty: Vec<String> = Vec::new();
        let result = handle_discord_webhook(&headers, &body, None, Some(&empty)).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Message(_, _)));
    }

    #[test]
    fn allowlist_matching_guild_allowed() {
        let body = slash_command_body(Some("guild_a"));
        let headers = HeaderMap::new();
        let list = vec!["guild_a".to_string(), "guild_b".to_string()];
        let result = handle_discord_webhook(&headers, &body, None, Some(&list)).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Message(_, _)));
    }

    #[test]
    fn allowlist_non_matching_guild_rejected() {
        let body = slash_command_body(Some("guild_c"));
        let headers = HeaderMap::new();
        let list = vec!["guild_a".to_string(), "guild_b".to_string()];
        let result = handle_discord_webhook(&headers, &body, None, Some(&list)).unwrap();
        match result {
            DiscordWebhookResult::GuildNotAllowed { guild_id } => {
                assert_eq!(guild_id, "guild_c");
            }
            other => panic!(
                "expected GuildNotAllowed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn allowlist_does_not_block_dms() {
        // DMs have no guild_id — allowlist should not apply
        let body = slash_command_body(None);
        let headers = HeaderMap::new();
        let list = vec!["guild_a".to_string()];
        let result = handle_discord_webhook(&headers, &body, None, Some(&list)).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Message(_, _)));
    }

    #[test]
    fn allowlist_does_not_block_ping() {
        // Discord's endpoint verification ping must always succeed regardless of allowlist
        let body = r#"{"id":"1","type":1,"token":"tok"}"#;
        let headers = HeaderMap::new();
        let list = vec!["guild_a".to_string()];
        let result = handle_discord_webhook(&headers, body, None, Some(&list)).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Pong(_)));
    }

    #[test]
    fn bot_user_returns_skip() {
        let body = r#"{
            "id": "3",
            "type": 2,
            "token": "tok",
            "data": { "name": "ask" },
            "user": { "id": "bot1", "username": "mybot", "bot": true }
        }"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Skip));
    }

    #[test]
    fn autocomplete_returns_skip() {
        let body = r#"{
            "id": "4",
            "type": 4,
            "token": "tok",
            "data": { "name": "search" },
            "member": { "user": { "id": "u1", "username": "alice" } }
        }"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Skip));
    }

    #[test]
    fn unknown_type_returns_skip() {
        let body = r#"{
            "id": "5",
            "type": 99,
            "token": "tok",
            "member": { "user": { "id": "u1", "username": "alice" } }
        }"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        assert!(matches!(result, DiscordWebhookResult::Skip));
    }

    #[test]
    fn message_component_returns_message() {
        let body = r#"{
            "id": "6",
            "type": 3,
            "token": "tok",
            "data": { "custom_id": "btn_yes" },
            "user": { "id": "u2", "username": "bob" },
            "channel_id": "ch2"
        }"#;
        let headers = HeaderMap::new();

        let result = handle_discord_webhook(&headers, body, None, None).unwrap();
        match result {
            DiscordWebhookResult::Message(msg, _) => {
                assert_eq!(msg.text, "btn_yes");
                assert_eq!(msg.sender_id, "u2");
            }
            _ => panic!("expected Message"),
        }
    }
}
