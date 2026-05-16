//! `NativeChannel` implementation for Telegram.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::{Mutex, RwLock};

use cortex::config::Config;
use cortex::db::Database;

use crate::channel::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::channel::channel_verbs::Channel;
use crate::constants::TELEGRAM_TYPING_KEEPALIVE_INTERVAL;
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

    fn prepare_response_context(
        &self,
        response_context: &mut serde_json::Value,
        db: Option<&Database>,
        config: &Config,
    ) -> Result<()> {
        let Some(db) = db else {
            return Ok(());
        };
        let mut ctx: TelegramResponseContext = serde_json::from_value(response_context.clone())?;
        ctx.voice_reply_mode = Some(crate::voice::resolve_voice_reply_mode(
            db,
            config,
            "telegram",
            &ctx.chat_id.to_string(),
        )?);
        *response_context = serde_json::to_value(ctx)?;
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

/// Routing target for `Channel::send` on Telegram.
///
/// Mirrors the parameter set the gateway already feeds to
/// `TelegramClient::send_message` so the trait path produces byte-identical
/// wire output to the existing direct-call sites.
#[derive(Debug, Clone)]
pub struct TelegramSendTarget {
    pub chat_id: i64,
    pub thread_id: Option<i64>,
    pub reply_to: Option<i64>,
}

impl TelegramSendTarget {
    pub fn new(chat_id: i64) -> Self {
        Self {
            chat_id,
            thread_id: None,
            reply_to: None,
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    type Target = TelegramSendTarget;

    fn name(&self) -> &'static str {
        "telegram"
    }

    fn keepalive_interval(&self) -> std::time::Duration {
        TELEGRAM_TYPING_KEEPALIVE_INTERVAL
    }

    fn verify(&self, headers: &HeaderMap, _body: &[u8]) -> Result<()> {
        let Some(secret) = self.secret.as_deref() else {
            return Ok(());
        };
        if super::verify::verify_secret_token(headers, secret) {
            Ok(())
        } else {
            anyhow::bail!("Telegram webhook secret token verification failed");
        }
    }

    async fn send(&self, target: &Self::Target, text: &str) -> Result<()> {
        self.client
            .send_message(
                target.chat_id,
                text,
                None,
                target.thread_id,
                target.reply_to,
            )
            .await
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

    #[test]
    fn prepare_response_context_reads_mode_from_provided_db() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        db.set_setting(
            &crate::voice::voice_mode_setting_key("telegram", "42"),
            crate::voice::VoiceReplyMode::VoiceOnly.as_str(),
        )
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
            .unwrap();

        let back: TelegramResponseContext = serde_json::from_value(response_context).unwrap();
        assert_eq!(
            back.voice_reply_mode,
            Some(crate::voice::VoiceReplyMode::VoiceOnly)
        );
    }

    fn make_channel(secret: Option<&str>) -> TelegramChannel {
        TelegramChannel {
            client: Arc::new(TelegramClient::new("token").unwrap()),
            dedup: Arc::new(Mutex::new(UpdateDeduplicator::new())),
            secret: secret.map(str::to_string),
            bot_username: None,
            config: Config::default(),
            tts_synthesizer: None,
        }
    }

    #[test]
    fn channel_trait_name_matches_native_channel_name() {
        let ch = make_channel(None);
        assert_eq!(Channel::name(&ch), "telegram");
        assert_eq!(NativeChannel::names(&ch), vec!["telegram"]);
    }

    #[test]
    fn channel_trait_keepalive_uses_telegram_constant() {
        let ch = make_channel(None);
        assert_eq!(
            Channel::keepalive_interval(&ch),
            TELEGRAM_TYPING_KEEPALIVE_INTERVAL
        );
    }

    #[test]
    fn verify_passes_when_no_secret_configured() {
        let ch = make_channel(None);
        let headers = HeaderMap::new();
        Channel::verify(&ch, &headers, b"{}").expect("missing secret means no verification");
    }

    #[test]
    fn verify_rejects_request_with_wrong_secret_token() {
        let ch = make_channel(Some("expected-secret"));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-telegram-bot-api-secret-token",
            axum::http::HeaderValue::from_static("wrong-secret"),
        );
        let err = Channel::verify(&ch, &headers, b"{}").unwrap_err();
        assert!(
            err.to_string().contains("verification failed"),
            "error must identify Telegram verification failure, got: {err}"
        );
    }

    #[test]
    fn verify_accepts_request_with_correct_secret_token() {
        let ch = make_channel(Some("expected-secret"));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-telegram-bot-api-secret-token",
            axum::http::HeaderValue::from_static("expected-secret"),
        );
        Channel::verify(&ch, &headers, b"{}").expect("matching secret must verify");
    }

    #[test]
    fn send_target_constructor_clears_optional_fields() {
        let target = TelegramSendTarget::new(42);
        assert_eq!(target.chat_id, 42);
        assert!(target.thread_id.is_none());
        assert!(target.reply_to.is_none());
    }
}
