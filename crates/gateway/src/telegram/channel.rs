//! `NativeChannel` implementation for Telegram.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::{Mutex, RwLock};

use borg_core::config::Config;

use crate::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;

use super::api::TelegramClient;
use super::dedup::UpdateDeduplicator;

/// Telegram-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct TelegramResponseContext {
    chat_id: i64,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
}

/// Telegram native channel implementation.
pub struct TelegramChannel {
    /// Shared Telegram Bot API HTTP client.
    pub client: Arc<TelegramClient>,
    /// Deduplicator to filter repeated webhook updates.
    pub dedup: Arc<Mutex<UpdateDeduplicator>>,
    /// Optional webhook secret token for request verification.
    pub secret: Option<String>,
    /// Bot username used for mention detection in groups.
    pub bot_username: Option<String>,
    /// Application configuration.
    pub config: Config,
    /// Optional TTS synthesizer for auto-voice responses.
    pub tts_synthesizer: Option<Arc<borg_core::tts::TtsSynthesizer>>,
}

#[async_trait]
impl NativeChannel for TelegramChannel {
    fn names(&self) -> Vec<&str> {
        vec!["telegram"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        // Verify and parse
        let parsed = match super::handle_telegram_webhook(
            headers,
            body,
            self.secret.as_deref(),
            &self.dedup,
        )
        .await
        {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return Ok(WebhookOutcome::Skip),
            Err(e) => return Err(e),
        };

        let super::TelegramParsed {
            mut inbound,
            audio: audio_ref,
            photo: photo_ref,
        } = parsed;

        // Photo download (must happen before enqueue — URLs expire, so we
        // inline the bytes as a base64 attachment exactly like Slack does).
        if let Some(ref photo) = photo_ref {
            match async {
                let file_info = self.client.get_file(&photo.file_id).await?;
                let file_path = file_info
                    .file_path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("No file_path in getFile response"))?;
                let bytes = self.client.download_file(file_path).await?;
                Ok::<_, anyhow::Error>(bytes)
            }
            .await
            {
                Ok(bytes) => {
                    use base64::Engine;
                    inbound.attachments.push(crate::handler::InboundAttachment {
                        mime_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        filename: Some(photo.filename.clone()),
                    });
                }
                Err(e) => {
                    tracing::warn!("Telegram photo download failed: {e}");
                }
            }
        }

        // Audio transcription (must happen before enqueue — needs file data)
        if let Some(ref audio) = audio_ref {
            if let Some(transcriber) =
                borg_core::media_understanding::AudioTranscriber::from_config(&self.config)
            {
                match async {
                    let file_info = self.client.get_file(&audio.file_id).await?;
                    let file_path = file_info
                        .file_path
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("No file_path in getFile response"))?;
                    let bytes = self.client.download_file(file_path).await?;
                    let filename = file_path
                        .rsplit('/')
                        .next()
                        .unwrap_or("audio.ogg")
                        .to_string();
                    let lang = self.config.audio.language.as_deref();
                    transcriber
                        .transcribe(&bytes, &audio.mime_type, &filename, lang)
                        .await
                }
                .await
                {
                    Ok((transcript, _attempts)) => {
                        inbound.text = format!("[Voice transcript]: {transcript}");
                        if self.config.audio.echo_transcript {
                            let echo_chat_id: i64 = inbound
                                .channel_id
                                .as_deref()
                                .and_then(|id| id.parse().ok())
                                .unwrap_or(0);
                            if echo_chat_id != 0 {
                                let _ = self
                                    .client
                                    .send_message(echo_chat_id, &transcript, None, None, None)
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Audio transcription failed: {e}");
                    }
                }
            }
        }

        let chat_id: i64 = match inbound.channel_id.as_deref().and_then(|id| id.parse().ok()) {
            Some(id) => id,
            None => {
                tracing::warn!("Missing or invalid chat_id in Telegram update");
                return Ok(WebhookOutcome::Skip);
            }
        };

        let thread_id: Option<i64> = inbound.thread_id.as_deref().and_then(|id| id.parse().ok());
        let reply_to: Option<i64> = inbound.message_id.as_deref().and_then(|id| id.parse().ok());

        let session_key =
            inbound.session_key("telegram", inbound.thread_id.as_deref().unwrap_or(""));

        let response_context = serde_json::to_value(TelegramResponseContext {
            chat_id,
            thread_id,
            reply_to,
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
        let ctx: TelegramResponseContext = serde_json::from_value(response_context.clone())?;

        let html = super::format::markdown_to_telegram_html(response_text);

        if let Err(e) = self
            .client
            .send_message(
                ctx.chat_id,
                &html,
                Some("HTML"),
                ctx.thread_id,
                ctx.reply_to,
            )
            .await
        {
            tracing::warn!("HTML send failed, retrying as plain text: {e}");
            if let Err(e2) = self
                .client
                .send_message(
                    ctx.chat_id,
                    response_text,
                    None,
                    ctx.thread_id,
                    ctx.reply_to,
                )
                .await
            {
                health
                    .write()
                    .await
                    .record_error("telegram", &e2.to_string());
                return Err(e2);
            }
        }
        health.write().await.record_outbound("telegram");

        // Auto-TTS: synthesize and send voice message after text
        if let Some(ref synth) = self.tts_synthesizer {
            let tts_text = borg_core::tts::truncate_for_tts(response_text, 4096);
            match synth
                .synthesize(&tts_text, None, Some(borg_core::tts::AudioFormat::Opus))
                .await
            {
                Ok((audio_bytes, _, _)) => {
                    if let Err(e) = self
                        .client
                        .send_voice(ctx.chat_id, &audio_bytes, None, ctx.thread_id, None)
                        .await
                    {
                        tracing::warn!("Failed to send TTS voice message: {e}");
                    }
                }
                Err(e) => tracing::warn!("TTS synthesis failed: {e}"),
            }
        }

        Ok(())
    }

    fn bot_mention(&self) -> Option<String> {
        self.bot_username.as_ref().map(|u| format!("@{u}"))
    }

    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let ctx: TelegramResponseContext = serde_json::from_value(response_context.clone()).ok()?;
        let client = self.client.clone();

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let config = crate::typing_keepalive::TypingKeepaliveConfig {
                keepalive_interval: std::time::Duration::from_secs(4),
                label: "telegram",
            };
            crate::typing_keepalive::run_keepalive(config, stop_rx, || {
                let client = client.clone();
                async move { client.send_typing(ctx.chat_id).await }
            })
            .await;
        });

        Some(TypingHandle::new(stop_tx, join))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_context_roundtrip() {
        let ctx = TelegramResponseContext {
            chat_id: 12345,
            thread_id: Some(67),
            reply_to: Some(89),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TelegramResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.chat_id, 12345);
        assert_eq!(back.thread_id, Some(67));
        assert_eq!(back.reply_to, Some(89));
    }

    #[test]
    fn response_context_minimal() {
        let ctx = TelegramResponseContext {
            chat_id: 1,
            thread_id: None,
            reply_to: None,
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TelegramResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.chat_id, 1);
        assert!(back.thread_id.is_none());
        assert!(back.reply_to.is_none());
    }
}
