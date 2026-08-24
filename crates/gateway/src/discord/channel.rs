//! `NativeChannel` implementation for Discord.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::{Mutex, RwLock};

use crate::channel::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

use super::api::DiscordClient;
use super::dedup::InteractionDeduplicator;
use super::DiscordWebhookResult;

/// Discord-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct DiscordResponseContext {
    channel_id: String,
    interaction_id: String,
    interaction_token: String,
    application_id: Option<String>,
}

/// Discord native channel implementation.
pub struct DiscordChannel {
    pub client: Arc<DiscordClient>,
    pub public_key: Option<String>,
    /// Optional guild allowlist. Empty or `None` means "allow all guilds".
    pub guild_allowlist: Option<Vec<String>>,
    /// Deduplicator for Discord interaction IDs. Discord retries interaction
    /// webhooks on non-2xx responses and transient network conditions; without
    /// this the agent can be invoked twice for the same interaction.
    pub dedup: Arc<Mutex<InteractionDeduplicator>>,
    /// Configuration handle — needed to build the STT pipeline so voice
    /// messages in channel context get transcribed before injection.
    pub config: cortex::config::Config,
}

impl DiscordChannel {
    pub fn new(
        client: Arc<DiscordClient>,
        public_key: Option<String>,
        guild_allowlist: Option<Vec<String>>,
        config: cortex::config::Config,
    ) -> Self {
        Self {
            client,
            public_key,
            guild_allowlist,
            dedup: Arc::new(Mutex::new(InteractionDeduplicator::new())),
            config,
        }
    }
}

#[async_trait]
impl NativeChannel for DiscordChannel {
    fn names(&self) -> Vec<&str> {
        vec!["discord"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        // Fail closed: refuse inbound interactions when no public key is
        // configured, rather than silently accepting unsigned requests.
        crate::verify_common::require_verification_secret(
            "discord",
            self.public_key.as_deref(),
            "DISCORD_PUBLIC_KEY",
        )?;
        let webhook_result = super::handle_discord_webhook(
            headers,
            body,
            self.public_key.as_deref(),
            self.guild_allowlist.as_deref(),
        )?;

        match webhook_result {
            DiscordWebhookResult::Pong(response) => Ok(WebhookOutcome::ProtocolResponse((
                StatusCode::OK,
                // InteractionResponse is a plain serde struct of primitives —
                // serialization is infallible. Fall back to a raw pong if it
                // ever does fail, which keeps Discord endpoint verification
                // working while surfacing the anomaly in logs.
                axum::Json(serde_json::to_value(response).unwrap_or_else(|e| {
                    tracing::error!("InteractionResponse serialization failed: {e}");
                    serde_json::json!({ "type": 1 })
                })),
            ))),
            DiscordWebhookResult::GuildNotAllowed { guild_id } => {
                tracing::info!(
                    discord_guild_id = %guild_id,
                    "Rejected Discord interaction from non-allowlisted guild"
                );
                Ok(WebhookOutcome::Skip)
            }
            DiscordWebhookResult::Skip => Ok(WebhookOutcome::Skip),
            DiscordWebhookResult::Message(mut inbound, interaction) => {
                // Deduplicate on interaction ID. Discord retries webhooks on
                // non-2xx responses and occasional transient conditions;
                // without this check the agent can be invoked twice for the
                // same interaction.
                {
                    let mut dedup = self.dedup.lock().await;
                    if dedup.is_duplicate(&interaction.id) {
                        tracing::debug!(
                            interaction_id = %interaction.id,
                            "Skipping duplicate Discord interaction"
                        );
                        return Ok(WebhookOutcome::Skip);
                    }
                }

                // Send deferred response immediately so Discord doesn't time out
                if let Err(e) = self
                    .client
                    .create_interaction_response(
                        &interaction.id,
                        &interaction.token,
                        &super::types::InteractionResponse::deferred(),
                    )
                    .await
                {
                    tracing::warn!("Failed to send Discord deferred response: {e}");
                }

                // Channel history context injection for guild (group) interactions.
                // Skip for DMs (no useful channel context) to save rate-limit budget.
                let is_guild = inbound
                    .peer_kind
                    .as_deref()
                    .is_some_and(|pk| pk == crate::constants::PEER_KIND_GROUP);
                if is_guild {
                    if let Some(ref channel_id) = inbound.channel_id {
                        match self.client.get_channel_messages(channel_id, 10).await {
                            Ok(history) if !history.is_empty() => {
                                // STT pipeline for voice messages in channel
                                // context. Discord voice attachments live at
                                // `audio/ogg` CDN URLs that need no auth.
                                let transcriber =
                                    cortex::media::understanding::AudioTranscriber::from_config(
                                        &self.config,
                                    );
                                let mut context = String::new();
                                for m in history.iter().rev() {
                                    let mut line_text = m.content.clone();
                                    if let Some(ref t) = transcriber {
                                        for att in &m.attachments {
                                            let mime = att.content_type.as_deref().unwrap_or(
                                                crate::constants::DEFAULT_ATTACHMENT_MIME,
                                            );
                                            if !mime.starts_with("audio/") {
                                                continue;
                                            }
                                            match self.client.http().get(&att.url).send().await {
                                                Ok(resp) if resp.status().is_success() => {
                                                    match resp.bytes().await {
                                                        Ok(bytes) => {
                                                            let fname = att
                                                                .filename
                                                                .clone()
                                                                .unwrap_or_else(|| {
                                                                    "discord-voice.audio"
                                                                        .to_string()
                                                                });
                                                            if let Some(transcript) =
                                                                voice::transcribe_or_warn(
                                                                    t, &bytes, mime, &fname,
                                                                    "discord",
                                                                )
                                                                .await
                                                            {
                                                                if !line_text.is_empty() {
                                                                    line_text.push(' ');
                                                                }
                                                                line_text.push_str(
                                                                    voice::VOICE_TRANSCRIPT_PREFIX,
                                                                );
                                                                line_text.push_str(&transcript);
                                                            }
                                                        }
                                                        Err(e) => {
                                                            tracing::warn!(error = %e, "discord: read attachment bytes failed")
                                                        }
                                                    }
                                                }
                                                Ok(resp) => tracing::warn!(
                                                    status = %resp.status(),
                                                    "discord: attachment fetch returned non-success"
                                                ),
                                                Err(e) => {
                                                    tracing::warn!(error = %e, "discord: attachment fetch failed")
                                                }
                                            }
                                        }
                                    }
                                    let line = format!("{}: {line_text}\n", m.author_username);
                                    if context.len() + line.len()
                                        > crate::constants::DISCORD_CHANNEL_CONTEXT_MAX_CHARS
                                    {
                                        break;
                                    }
                                    context.push_str(&line);
                                }
                                if !context.is_empty() {
                                    inbound.text = format!(
                                        "[Channel context]\n{context}[Current message]\n{}",
                                        inbound.text
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to fetch Discord channel history for context: {e}"
                                );
                            }
                        }
                    }
                }

                let session_key =
                    inbound.session_key("discord", inbound.channel_id.as_deref().unwrap_or(""));

                let response_context = serde_json::to_value(DiscordResponseContext {
                    channel_id: inbound.channel_id.clone().unwrap_or_default(),
                    interaction_id: interaction.id.clone(),
                    interaction_token: interaction.token.clone(),
                    application_id: interaction.application_id.clone(),
                })?;

                Ok(WebhookOutcome::Message {
                    inbound,
                    session_key,
                    response_context,
                })
            }
        }
    }

    async fn send_response(
        &self,
        response_text: &str,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        let ctx: DiscordResponseContext = serde_json::from_value(response_context.clone())?;

        // Discord has no per-message reply threading we can rely on for /btw,
        // so we mark the answer with a visual prefix instead. Stripped/no-op
        // when not a /btw reply.
        let response_text = crate::btw_dispatch::maybe_prefix_btw(response_text, response_context);
        let formatted = super::format::markdown_to_discord(&response_text);

        if let Some(ref app_id) = ctx.application_id {
            if let Err(e) = self
                .client
                .edit_original_response(app_id, &ctx.interaction_token, &formatted)
                .await
            {
                tracing::warn!("Failed to edit Discord interaction response: {e}");
                // Fallback: send as channel message
                if let Err(e2) = self.client.send_message(&ctx.channel_id, &formatted).await {
                    health
                        .write()
                        .await
                        .record_error("discord", &e2.to_string());
                    return Err(e2);
                }
            }
        } else if let Err(e) = self.client.send_message(&ctx.channel_id, &formatted).await {
            health.write().await.record_error("discord", &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound("discord");
        Ok(())
    }

    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let ctx: DiscordResponseContext = serde_json::from_value(response_context.clone()).ok()?;
        let client = self.client.clone();

        Some(TypingHandle::spawn_keepalive(
            crate::typing_keepalive::TypingKeepaliveConfig {
                keepalive_interval: std::time::Duration::from_secs(8),
                label: "discord",
            },
            move || {
                let client = client.clone();
                let ch = ctx.channel_id.clone();
                async move { client.trigger_typing_indicator(&ch).await }
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use ed25519_dalek::{Signer, SigningKey};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_signed_headers(signing_key: &SigningKey, body: &str) -> (HeaderMap, String) {
        let timestamp = chrono::Utc::now().timestamp().to_string();
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
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        (headers, public_key_hex)
    }

    fn channel_against(server: &MockServer, public_key_hex: String) -> DiscordChannel {
        DiscordChannel::new(
            Arc::new(DiscordClient::with_base("tok-1", &server.uri()).unwrap()),
            Some(public_key_hex),
            None,
            cortex::config::Config::default(),
        )
    }

    /// Signed slash-command body. `guild_id: Some` makes it a guild (group)
    /// interaction; `None` makes it a DM.
    fn command_body(guild_id: Option<&str>) -> String {
        let mut body = serde_json::json!({
            "id": "9001",
            "type": 2,
            "token": "t0k",
            "application_id": "app1",
            "channel_id": "123",
            "data": { "name": "ask", "options": [{ "name": "q", "value": "hello borg" }] },
        });
        match guild_id {
            Some(g) => {
                body["guild_id"] = serde_json::json!(g);
                body["member"] = serde_json::json!({
                    "user": { "id": "u1", "username": "alice", "bot": false }
                });
            }
            None => {
                body["user"] = serde_json::json!({
                    "id": "u1", "username": "alice", "bot": false
                });
            }
        }
        body.to_string()
    }

    async fn run_webhook(channel: &DiscordChannel, headers: &HeaderMap, body: &str) -> String {
        let config = cortex::config::Config::default();
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        match channel.handle_webhook(headers, body, &ctx).await.unwrap() {
            WebhookOutcome::Message { inbound, .. } => inbound.text,
            _ => panic!("expected Message outcome"),
        }
    }

    #[test]
    fn names_is_exactly_discord() {
        // The registry keys webhook dispatch on this name — "discord" must
        // be present and nothing else may claim the channel.
        let channel = DiscordChannel::new(
            Arc::new(DiscordClient::new("tok").unwrap()),
            None,
            None,
            cortex::config::Config::default(),
        );
        assert_eq!(channel.names(), vec!["discord"]);
    }

    #[tokio::test]
    async fn guild_interaction_injects_channel_history_context() {
        let server = MockServer::start().await;
        // Deferred callback the handler fires before fetching history.
        Mock::given(method("POST"))
            .and(path("/interactions/9001/t0k/callback"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        // Recent channel messages (newest first, as the API returns them).
        Mock::given(method("GET"))
            .and(path("/channels/123/messages"))
            .and(query_param("limit", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "m2", "content": "newer msg", "author": {"id": "u2", "username": "bob"} },
                { "id": "m1", "content": "older msg", "author": {"id": "u3", "username": "carol"} }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let body = command_body(Some("g1"));
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (headers, pub_hex) = make_signed_headers(&signing_key, &body);
        let channel = channel_against(&server, pub_hex);

        let text = run_webhook(&channel, &headers, &body).await;
        assert!(text.contains("[Channel context]"), "{text}");
        // Oldest-first rendering of the fetched history.
        assert!(text.contains("carol: older msg\nbob: newer msg"), "{text}");
        assert!(text.contains("[Current message]\nhello borg"), "{text}");
    }

    #[tokio::test]
    async fn dm_interaction_skips_channel_history_fetch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/interactions/9001/t0k/callback"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        // DMs must not spend rate-limit budget on history fetches.
        Mock::given(method("GET"))
            .and(path("/channels/123/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0)
            .mount(&server)
            .await;

        let body = command_body(None);
        let signing_key = SigningKey::from_bytes(&[8u8; 32]);
        let (headers, pub_hex) = make_signed_headers(&signing_key, &body);
        let channel = channel_against(&server, pub_hex);

        let text = run_webhook(&channel, &headers, &body).await;
        assert_eq!(text, "hello borg");
    }

    #[test]
    fn response_context_roundtrip() {
        let ctx = DiscordResponseContext {
            channel_id: "ch1".to_string(),
            interaction_id: "int1".to_string(),
            interaction_token: "tok1".to_string(),
            application_id: Some("app1".to_string()),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: DiscordResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_id, "ch1");
        assert_eq!(back.application_id.as_deref(), Some("app1"));
    }
}
