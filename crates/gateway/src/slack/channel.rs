//! `NativeChannel` implementation for Slack.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::{Mutex, RwLock};

use crate::channel::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

use super::api::SlackClient;
use super::dedup::EventDeduplicator;
use super::SlackWebhookResult;

/// Slack-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct SlackResponseContext {
    channel_id: String,
    thread_ts: Option<String>,
    message_ts: Option<String>,
}

/// Slack native channel implementation.
pub struct SlackChannel {
    pub client: Arc<SlackClient>,
    pub signing_secret: Option<String>,
    pub dedup: Arc<Mutex<EventDeduplicator>>,
    pub bot_user_id: Option<String>,
    /// Configuration handle — needed to build the STT pipeline for
    /// inbound voice/audio file transcription (parity with Telegram).
    pub config: cortex::config::Config,
}

#[async_trait]
impl NativeChannel for SlackChannel {
    fn names(&self) -> Vec<&str> {
        vec!["slack"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        // Fail closed: refuse inbound traffic when no signing secret is
        // configured, rather than silently accepting unauthenticated requests.
        crate::verify_common::require_verification_secret(
            "slack",
            self.signing_secret.as_deref(),
            "SLACK_SIGNING_SECRET",
        )?;
        let webhook_result = super::handle_slack_webhook(
            headers,
            body,
            self.signing_secret.as_deref(),
            Some(&self.dedup),
            self.client.bot_user_id(),
            Some(self.client.echo_cache()),
        )
        .await?;

        let mut inbound = match webhook_result {
            SlackWebhookResult::Challenge(challenge) => {
                return Ok(WebhookOutcome::ProtocolResponse((
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "challenge": challenge })),
                )));
            }
            SlackWebhookResult::Skip => return Ok(WebhookOutcome::Skip),
            SlackWebhookResult::Message(inbound) => *inbound,
        };

        // Ack reaction — immediate visual feedback
        if let (Some(ref ch), Some(ref ts)) = (&inbound.channel_id, &inbound.message_id) {
            self.client.add_reaction(ch, ts, "eyes").await;
        }

        // Download file attachments (placeholder URLs → base64 data).
        // Voice/audio files take a side-path through the STT pipeline so
        // the agent sees `[Voice transcript]: …` in the message text
        // (parity with Telegram + Twilio inbound voice).
        let transcriber = cortex::media::understanding::AudioTranscriber::from_config(&self.config);
        let mut resolved_attachments = Vec::new();
        let mut voice_transcripts: Vec<String> = Vec::new();
        for att in &inbound.attachments {
            if att.data.starts_with("https://") {
                match self.client.download_file(&att.data).await {
                    Ok((bytes, content_type)) => {
                        if content_type.starts_with("audio/") {
                            if let Some(ref t) = transcriber {
                                let filename = att
                                    .filename
                                    .clone()
                                    .unwrap_or_else(|| "slack-voice.audio".to_string());
                                voice_transcripts.extend(
                                    voice::transcribe_or_warn(
                                        t,
                                        &bytes,
                                        &content_type,
                                        &filename,
                                        "slack",
                                    )
                                    .await,
                                );
                            }
                        }
                        use base64::Engine;
                        resolved_attachments.push(crate::handler::InboundAttachment {
                            mime_type: content_type,
                            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            filename: att.filename.clone(),
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to download Slack file {}: {e}",
                            att.filename.as_deref().unwrap_or("unknown")
                        );
                    }
                }
            } else {
                resolved_attachments.push(att.clone());
            }
        }
        inbound.attachments = resolved_attachments;
        voice::append_voice_transcripts(&mut inbound.text, &voice_transcripts);

        // Only inject channel history for top-level @mentions: a threaded reply
        // already carries its own conversation context, so re-injecting full
        // channel history would duplicate it and bloat the prompt.
        if inbound.text.contains("<@") && inbound.thread_ts.is_none() {
            if let Some(ref channel) = inbound.channel_id {
                match self.client.conversations_history(channel, 10).await {
                    Ok(history) if !history.is_empty() => {
                        // Cap injected history so it never dominates the agent's token budget.
                        const MAX_CONTEXT_CHARS: usize = 8000;
                        let mut context = String::new();
                        for m in history.iter().rev() {
                            let line = format!("<@{}>: {}\n", m.user, m.text);
                            if context.len() + line.len() > MAX_CONTEXT_CHARS {
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
                        tracing::warn!("Failed to fetch channel history for context: {e}");
                    }
                }
            }
        }

        let session_key = inbound.session_key("slack", inbound.thread_id.as_deref().unwrap_or(""));

        let response_context = serde_json::to_value(SlackResponseContext {
            channel_id: inbound.channel_id.clone().unwrap_or_default(),
            thread_ts: inbound.thread_ts.clone(),
            message_ts: inbound.message_id.clone(),
        })?;

        Ok(WebhookOutcome::Message {
            inbound,
            session_key,
            response_context,
        })
    }

    async fn send_response(
        &self,
        response_text: &str,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        let ctx: SlackResponseContext = serde_json::from_value(response_context.clone())?;

        let formatted = super::format::markdown_to_mrkdwn(response_text);

        // /btw replies must land in a thread off the inbound message even when
        // the user wrote `/btw …` at the top level of a channel — otherwise the
        // side question pollutes the channel's main timeline.
        let thread_ts = if crate::btw_dispatch::is_threaded(response_context) {
            let resolved = ctx.thread_ts.as_deref().or(ctx.message_ts.as_deref());
            if resolved.is_none() {
                tracing::warn!(
                    "/btw on Slack with no thread_ts AND no message_ts — answer will post top-level"
                );
            }
            resolved
        } else {
            ctx.thread_ts.as_deref()
        };

        if let Err(e) = self
            .client
            .post_message(&ctx.channel_id, &formatted, thread_ts)
            .await
        {
            health.write().await.record_error("slack", &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound("slack");

        // Replace ack reaction with done reaction
        if let Some(ref ts) = ctx.message_ts {
            self.client
                .remove_reaction(&ctx.channel_id, ts, "eyes")
                .await;
            self.client
                .add_reaction(&ctx.channel_id, ts, "white_check_mark")
                .await;
        }

        Ok(())
    }

    fn bot_mention(&self) -> Option<String> {
        self.bot_user_id.as_ref().map(|id| format!("<@{id}>"))
    }

    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let ctx: SlackResponseContext = serde_json::from_value(response_context.clone()).ok()?;
        let client = self.client.clone();

        Some(TypingHandle::spawn_keepalive(
            crate::typing_keepalive::TypingKeepaliveConfig {
                // Re-send before Slack's typing indicator decays (~5s) so it stays visible.
                keepalive_interval: std::time::Duration::from_secs(3),
                label: "slack",
            },
            move || {
                let client = client.clone();
                let ch = ctx.channel_id.clone();
                let ts = ctx.thread_ts.clone();
                async move {
                    client
                        .set_thread_status(&ch, ts.as_deref(), "is typing...")
                        .await
                }
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_context_roundtrip() {
        let ctx = SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: Some("1234567890.123456".to_string()),
            message_ts: Some("1234567890.654321".to_string()),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: SlackResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_id, "C123");
        assert_eq!(back.thread_ts.as_deref(), Some("1234567890.123456"));
        assert_eq!(back.message_ts.as_deref(), Some("1234567890.654321"));
    }

    /// `/btw` arriving at the top level of a channel must fall back to the
    /// inbound `message_ts` so the answer lands in a thread under the user's
    /// question instead of polluting the main channel timeline. Mirrors the
    /// fallback applied inside `send_response`.
    #[test]
    fn btw_thread_anchor_falls_back_to_message_ts() {
        let mut ctx = serde_json::to_value(SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: None,
            message_ts: Some("1700000000.111111".to_string()),
        })
        .unwrap();
        crate::btw_dispatch::mark_threaded(&mut ctx);

        let parsed: SlackResponseContext = serde_json::from_value(ctx.clone()).unwrap();
        let resolved = if crate::btw_dispatch::is_threaded(&ctx) {
            parsed.thread_ts.as_deref().or(parsed.message_ts.as_deref())
        } else {
            parsed.thread_ts.as_deref()
        };
        assert_eq!(resolved, Some("1700000000.111111"));
    }

    /// Without the threading flag, top-level posts stay top-level. Guards
    /// against an over-aggressive fallback that would re-thread every reply.
    #[test]
    fn no_thread_anchor_preserves_top_level_posting() {
        let ctx = serde_json::to_value(SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: None,
            message_ts: Some("1700000000.111111".to_string()),
        })
        .unwrap();

        let parsed: SlackResponseContext = serde_json::from_value(ctx.clone()).unwrap();
        let resolved = if crate::btw_dispatch::is_threaded(&ctx) {
            parsed.thread_ts.as_deref().or(parsed.message_ts.as_deref())
        } else {
            parsed.thread_ts.as_deref()
        };
        assert_eq!(resolved, None);
    }
}
