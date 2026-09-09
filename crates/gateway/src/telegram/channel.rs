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
        // Fail closed: refuse inbound updates when no webhook secret token is
        // configured. Telegram's secret token is protocol-optional, but without
        // it any host that learns the webhook URL can POST forged updates.
        crate::verify_common::require_verification_secret(
            "telegram",
            self.secret.as_deref(),
            "TELEGRAM_WEBHOOK_SECRET",
        )?;
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

    fn supports_media_attachments(&self) -> bool {
        true
    }

    async fn send_media(
        &self,
        path: &std::path::Path,
        caption: Option<&str>,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        use anyhow::Context as _;

        let ctx: TelegramResponseContext = serde_json::from_value(response_context.clone())?;
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read media file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        // Telegram caps captions at 1024 UTF-16 code units (not chars —
        // emoji count double); truncate rather than let the whole send 400.
        let caption = caption.map(|c| crate::message_processing::chunker::truncate_utf16(c, 1024));
        let source = super::types::MediaSource::Bytes {
            bytes: &bytes,
            filename,
            mime: None,
        };

        let result = if super::delivery::is_photo_path(path) {
            self.client
                .send_photo(ctx.chat_id, source, caption.as_deref(), ctx.thread_id, None)
                .await
        } else {
            self.client
                .send_document(ctx.chat_id, source, caption.as_deref(), ctx.thread_id, None)
                .await
        };
        match result {
            Ok(()) => {
                health.write().await.record_outbound("telegram");
                Ok(())
            }
            Err(e) => {
                health
                    .write()
                    .await
                    .record_error("telegram", &e.to_string());
                Err(e)
            }
        }
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

    fn channel_with_client(client: TelegramClient, bot_username: Option<&str>) -> TelegramChannel {
        TelegramChannel {
            client: Arc::new(client),
            dedup: Arc::new(Mutex::new(UpdateDeduplicator::new())),
            secret: None,
            bot_username: bot_username.map(String::from),
            config: Config::default(),
            tts_synthesizer: None,
        }
    }

    fn context_json(chat_id: i64) -> serde_json::Value {
        serde_json::to_value(TelegramResponseContext {
            chat_id,
            thread_id: None,
            reply_to: None,
            input_was_voice: false,
            voice_reply_mode: None,
        })
        .unwrap()
    }

    /// The channel registers under exactly "telegram" — routing, health
    /// records, and per-channel bindings all key on that name — and it
    /// advertises media support (Telegram accepts photo/document uploads).
    #[test]
    fn names_and_capabilities_identify_telegram() {
        let channel = channel_with_client(TelegramClient::new("123:ABC").unwrap(), None);
        assert_eq!(channel.names(), vec!["telegram"]);
        assert!(channel.supports_media_attachments());
    }

    /// Group mention detection needs the bot's username prefixed with `@`;
    /// without a configured username there is nothing to match on.
    #[test]
    fn bot_mention_prefixes_the_configured_username() {
        let with_name =
            channel_with_client(TelegramClient::new("123:ABC").unwrap(), Some("borg_bot"));
        assert_eq!(with_name.bot_mention(), Some("@borg_bot".to_string()));

        let without = channel_with_client(TelegramClient::new("123:ABC").unwrap(), None);
        assert_eq!(without.bot_mention(), None);
    }

    /// `send_response` must actually deliver through the API: a successful
    /// send posts sendMessage with the context's chat_id and records an
    /// outbound event; a rejected send must surface as an error, never a
    /// silent Ok for a reply that never arrived.
    #[tokio::test]
    async fn send_response_delivers_and_surfaces_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Success direction.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let channel = channel_with_client(
            TelegramClient::with_base("123:ABC", &server.uri()).unwrap(),
            None,
        );
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        channel
            .send_response("hello there", &context_json(77), &health)
            .await
            .expect("delivery must succeed on ok=true");
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["chat_id"], 77, "must send to the context's chat");
        let snapshot = health.read().await.snapshot();
        let telegram = snapshot.iter().find(|s| s.name == "telegram").unwrap();
        assert_eq!(telegram.outbound_count, 1, "success must record outbound");

        // Failure direction: both the HTML attempt and the plain-text
        // fallback are rejected, so the error must propagate.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: chat not found"
            })))
            .mount(&server)
            .await;
        let channel = channel_with_client(
            TelegramClient::with_base("123:ABC", &server.uri()).unwrap(),
            None,
        );
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let err = channel
            .send_response("hello there", &context_json(77), &health)
            .await
            .expect_err("a rejected send must not report success");
        assert!(
            err.to_string().contains("chat not found"),
            "API description must surface, got: {err}"
        );
    }

    /// `send_media` must read the file, route photos to sendPhoto, and
    /// surface API failures — a no-op impl would report success for an
    /// attachment that never left the machine.
    #[tokio::test]
    async fn send_media_routes_photos_and_surfaces_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("pic.jpg");
        std::fs::write(&photo_path, b"jpeg-bytes").unwrap();
        let doc_path = dir.path().join("report.pdf");
        std::fs::write(&doc_path, b"pdf-bytes").unwrap();

        // Photo extension routes to sendPhoto and succeeds.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendPhoto"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let channel = channel_with_client(
            TelegramClient::with_base("123:ABC", &server.uri()).unwrap(),
            None,
        );
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        channel
            .send_media(&photo_path, Some("a caption"), &context_json(77), &health)
            .await
            .expect("photo upload must succeed on ok=true");
        let snapshot = health.read().await.snapshot();
        let telegram = snapshot.iter().find(|s| s.name == "telegram").unwrap();
        assert_eq!(telegram.outbound_count, 1, "success must record outbound");

        // Non-photo extension routes to sendDocument; a rejection must
        // propagate and be recorded against channel health.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendDocument"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: document too large"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let channel = channel_with_client(
            TelegramClient::with_base("123:ABC", &server.uri()).unwrap(),
            None,
        );
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let err = channel
            .send_media(&doc_path, None, &context_json(77), &health)
            .await
            .expect_err("a rejected upload must not report success");
        assert!(
            err.to_string().contains("document too large"),
            "API description must surface, got: {err}"
        );
        let snapshot = health.read().await.snapshot();
        let telegram = snapshot.iter().find(|s| s.name == "telegram").unwrap();
        assert_eq!(telegram.error_count, 1, "failure must record the error");
    }

    /// A valid response context yields a live typing handle (the keepalive
    /// is what makes long turns feel alive on Telegram); garbage context
    /// yields none instead of panicking the dispatcher.
    #[tokio::test]
    async fn start_typing_yields_a_handle_for_valid_context() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .mount(&server)
            .await;
        let channel = channel_with_client(
            TelegramClient::with_base("123:ABC", &server.uri()).unwrap(),
            None,
        );

        let handle = channel.start_typing(&context_json(77));
        assert!(handle.is_some(), "valid context must start the keepalive");
        drop(handle);

        assert!(
            channel
                .start_typing(&serde_json::json!({ "not": "a telegram context" }))
                .is_none(),
            "an undecodable context must yield no handle"
        );
    }

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
