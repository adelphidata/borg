//! `NativeChannel` implementation for Telegram.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::{Mutex, RwLock};

use cortex::config::Config;
use cortex::db::Database;

use crate::channel::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;

use super::api::TelegramClient;
use super::dedup::UpdateDeduplicator;

/// Telegram-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct TelegramResponseContext {
    chat_id: i64,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
    input_was_voice: bool,
    voice_reply_mode: Option<crate::voice::VoiceReplyMode>,
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
    pub tts_synthesizer: Option<Arc<cortex::media::tts::TtsSynthesizer>>,
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

        let mut parsed = parsed;
        super::enrich_parsed_message(&self.client, &self.config, &mut parsed).await;
        let inbound = parsed.inbound;

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
            input_was_voice: matches!(parsed.input_kind, super::parse::TelegramInputKind::Voice),
            voice_reply_mode: None,
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

        // /btw answers ride Telegram's native quote-reply (`reply_to_message_id`
        // is already set on every inbound), so threading is automatic. The
        // visual prefix lets the user distinguish a side answer from the main
        // thread.
        let response_text = crate::btw_dispatch::maybe_prefix_btw(response_text, response_context);

        super::delivery::send_response(
            &self.client,
            ctx.chat_id,
            &response_text,
            ctx.thread_id,
            ctx.reply_to,
            ctx.input_was_voice,
            ctx.voice_reply_mode,
            health,
            self.tts_synthesizer.as_deref(),
        )
        .await
    }

    async fn prepare_response_context(
        &self,
        response_context: &mut serde_json::Value,
        db: Option<&Database>,
        config: &Config,
    ) -> Result<()> {
        let Some(db) = db else {
            return Ok(());
        };
        let mut ctx: TelegramResponseContext = serde_json::from_value(response_context.clone())?;
        ctx.voice_reply_mode = Some(
            crate::voice::resolve_voice_reply_mode(
                db,
                config,
                "telegram",
                &ctx.chat_id.to_string(),
            )
            .await?,
        );
        *response_context = serde_json::to_value(ctx)?;
        Ok(())
    }

    fn bot_mention(&self) -> Option<String> {
        self.bot_username.as_ref().map(|u| format!("@{u}"))
    }

    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let ctx: TelegramResponseContext = serde_json::from_value(response_context.clone()).ok()?;
        Some(super::typing::start_typing_keepalive(
            self.client.clone(),
            ctx.chat_id,
        ))
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
            input_was_voice: true,
            voice_reply_mode: Some(crate::voice::VoiceReplyMode::VoiceOnly),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TelegramResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.chat_id, 12345);
        assert_eq!(back.thread_id, Some(67));
        assert_eq!(back.reply_to, Some(89));
        assert!(back.input_was_voice);
        assert_eq!(
            back.voice_reply_mode,
            Some(crate::voice::VoiceReplyMode::VoiceOnly)
        );
    }

    #[test]
    fn response_context_minimal() {
        let ctx = TelegramResponseContext {
            chat_id: 1,
            thread_id: None,
            reply_to: None,
            input_was_voice: false,
            voice_reply_mode: None,
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TelegramResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.chat_id, 1);
        assert!(back.thread_id.is_none());
        assert!(back.reply_to.is_none());
        assert!(!back.input_was_voice);
        assert!(back.voice_reply_mode.is_none());
    }

    #[tokio::test]
    async fn prepare_response_context_reads_mode_from_provided_db() {
        let db = Database::test_db().await;
        db.set_setting(
            &crate::voice::voice_mode_setting_key("telegram", "42"),
            crate::voice::VoiceReplyMode::VoiceOnly.as_str(),
        )
        .await
        .unwrap();
        let channel = TelegramChannel {
            client: Arc::new(TelegramClient::new("token").unwrap()),
            dedup: Arc::new(Mutex::new(UpdateDeduplicator::new())),
            secret: None,
            bot_username: None,
            config: Config::default(),
            tts_synthesizer: None,
        };
        let mut response_context = serde_json::to_value(TelegramResponseContext {
            chat_id: 42,
            thread_id: None,
            reply_to: None,
            input_was_voice: true,
            voice_reply_mode: None,
        })
        .unwrap();

        channel
            .prepare_response_context(&mut response_context, Some(&db), &Config::default())
            .await
            .unwrap();

        let back: TelegramResponseContext = serde_json::from_value(response_context).unwrap();
        assert_eq!(
            back.voice_reply_mode,
            Some(crate::voice::VoiceReplyMode::VoiceOnly)
        );
    }
}
