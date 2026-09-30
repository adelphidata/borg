use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use tracing::warn;

use super::types::{
    ApiResponse, DeleteMessageRequest, EditMessageTextRequest, FileInfo, InlineKeyboardMarkup,
    MediaSource, ReactionType, SendMessageRequest, SendPollRequest, SetMessageReactionRequest,
    Update, User,
};
use crate::commands::{CommandDef, NativeCommandRegistration};
use crate::constants::{
    DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT, TELEGRAM_MAX_FILE_SIZE_BYTES,
};
use crate::message_processing::chunker;
use crate::resilience::circuit_breaker::CircuitBreaker;

const TELEGRAM_API_BASE: &str = crate::constants::TELEGRAM_API_BASE;
const HTTP_CONNECT_TIMEOUT: Duration = crate::constants::TELEGRAM_HTTP_CONNECT_TIMEOUT;
const MEDIA_SEND_TIMEOUT: Duration = crate::constants::TELEGRAM_MEDIA_SEND_TIMEOUT;

// Telegram returns retry_after in JSON body, not Retry-After header, so we use custom retry logic instead of http_retry.
const MAX_SEND_RETRIES: u32 = crate::constants::TELEGRAM_MAX_SEND_RETRIES;
const FLOOD_FAIL_FAST_SECS: u64 = crate::constants::TELEGRAM_FLOOD_FAIL_FAST_SECS;

/// Longest per-chat flood cooldown that the client keeps, whatever wait
/// Telegram names.
const FLOOD_COOLDOWN_CAP: Duration = Duration::from_secs(300);

/// A `sendMessage` failure that the caller must tell apart from the rest.
///
/// Attached to the `anyhow` error so callers can `downcast_ref` it. All other
/// failures (transport, auth, circuit breaker) stay untyped.
#[derive(Debug, thiserror::Error)]
pub enum TelegramSendError {
    /// Telegram refused the markup of the text. The same text sent with no
    /// parse mode can succeed.
    #[error("{0}")]
    Markup(String),
    /// Telegram flood control refused the send. A second call inside the
    /// window can extend the penalty.
    #[error("{message}")]
    Flood {
        /// Seconds until Telegram accepts a send to this chat again.
        retry_after_secs: u64,
        /// Text of the error.
        message: String,
    },
}

/// True when Telegram refused a send because it could not parse the markup.
fn is_markup_rejection(status: u16, description: &str) -> bool {
    status == 400 && description.to_lowercase().contains("can't parse entities")
}

/// A client for the Telegram Bot API.
#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    token: String,
    base_url: String,
    circuit_breaker: Arc<CircuitBreaker>,
    /// Per-chat time before which text sends are refused locally, set when
    /// Telegram flood control refuses a send. Shared between clones.
    flood_until: Arc<Mutex<HashMap<i64, Instant>>>,
}

impl TelegramClient {
    pub fn new(token: &str) -> Result<Self> {
        Self::with_base(token, TELEGRAM_API_BASE)
    }

    /// Construct against a non-default API base. Production always uses
    /// [`TELEGRAM_API_BASE`]; tests point this at a local mock server.
    pub(crate) fn with_base(token: &str, base_url: &str) -> Result<Self> {
        Ok(Self {
            client: crate::resilience::http_retry::build_channel_client(
                "Telegram",
                GATEWAY_HTTP_TIMEOUT,
                Some(HTTP_CONNECT_TIMEOUT),
            )?,
            token: token.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                cortex::constants::TELEGRAM_CIRCUIT_FAILURE_THRESHOLD,
                cortex::constants::TELEGRAM_CIRCUIT_SUSPENSION_SECS,
            )),
            flood_until: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Seconds that remain of the flood cooldown of a chat, if one is active.
    /// An expired entry is removed.
    fn flood_cooldown_remaining(&self, chat_id: i64) -> Option<u64> {
        let mut map = self
            .flood_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = map.get(&chat_id)?.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            map.remove(&chat_id);
            return None;
        }
        // Round up, so a part of a second does not report as 0.
        Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0))
    }

    /// Record a flood refusal for a chat and build the typed error.
    ///
    /// The same call removes the expired entries of all chats. A chat that
    /// does not send again never reaches the removal in
    /// `flood_cooldown_remaining`, so without this the map grows for the
    /// life of the process.
    fn flood_error(&self, chat_id: i64, retry_after_secs: u64, message: String) -> anyhow::Error {
        let wait = Duration::from_secs(retry_after_secs).min(FLOOD_COOLDOWN_CAP);
        let now = Instant::now();
        {
            let mut map = self
                .flood_until
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.retain(|_, until| *until > now);
            map.insert(chat_id, now + wait);
        }
        anyhow::Error::new(TelegramSendError::Flood {
            retry_after_secs,
            message,
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base_url, self.token)
    }

    fn file_url(&self, file_path: &str) -> String {
        format!("{}/file/bot{}/{file_path}", self.base_url, self.token)
    }

    /// Scrub the bot token out of arbitrary error text.
    fn scrub(&self, text: &str) -> String {
        redact_bot_token(text, &self.token)
    }

    /// Flatten a transport error into a token-scrubbed `anyhow` error.
    ///
    /// `reqwest` error Displays embed the request URL, which contains the bot
    /// token (`…/bot<token>/<method>`). The error is flattened to a plain
    /// redacted string — deliberately dropping the source chain — so no
    /// downstream consumer (`tracing`, the channel-health registry, `{:#}`
    /// chain formatting) can ever surface the raw token.
    fn scrub_err(&self, e: reqwest::Error, action: &str) -> anyhow::Error {
        let chain = format!("{:#}", anyhow::Error::from(e));
        anyhow::anyhow!("{action}: {}", self.scrub(&chain))
    }

    /// Validate the bot token by calling getMe.
    pub async fn get_me(&self) -> Result<User> {
        let resp: ApiResponse<User> = self
            .client
            .get(self.api_url("getMe"))
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call getMe"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse getMe response"))?;

        match resp.result {
            Some(user) if resp.ok => Ok(user),
            _ => bail!(
                "getMe failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            ),
        }
    }

    /// Send a text message, automatically chunking if it exceeds 4000 chars.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        // Telegram's 4096 cap is UTF-16 code units; the char-budgeted chunks
        // need a re-split when astral-plane content makes units outrun chars.
        let chunks = chunker::resplit_utf16(
            chunker::chunks_for_delivery(text, DEFAULT_MESSAGE_CHUNK_SIZE)?,
            DEFAULT_MESSAGE_CHUNK_SIZE,
        );

        for (i, chunk) in chunks.iter().enumerate() {
            // Only reply to the original message on the first chunk
            let reply_id = if i == 0 { reply_to_message_id } else { None };
            self.send_single_message(chat_id, chunk, parse_mode, message_thread_id, reply_id)
                .await?;
        }
        Ok(())
    }

    async fn send_single_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        if self.circuit_breaker.is_open() {
            bail!("Telegram circuit breaker open, skipping send_message");
        }
        if let Some(remaining) = self.flood_cooldown_remaining(chat_id) {
            // Telegram refused this chat a moment ago. A call inside the
            // window can extend the penalty, so make none.
            return Err(anyhow::Error::new(TelegramSendError::Flood {
                retry_after_secs: remaining,
                message: format!(
                    "sendMessage flood-limited for {remaining}s — chat is in flood cooldown, no request sent"
                ),
            }));
        }

        let body = SendMessageRequest {
            chat_id,
            text: text.to_string(),
            parse_mode: parse_mode.map(String::from),
            reply_to_message_id,
            message_thread_id,
            reply_markup: None,
            disable_notification: None,
        };

        let mut attempts = 0u32;

        loop {
            let send_result = self
                .client
                .post(self.api_url("sendMessage"))
                .json(&body)
                .send()
                .await;

            let resp = match send_result {
                Ok(r) => r,
                Err(e) => {
                    if is_safe_to_retry(&e) {
                        let scrubbed = self.scrub(&e.to_string());
                        if retries_exhausted(&mut attempts) {
                            bail!(
                                "sendMessage failed after {MAX_SEND_RETRIES} retries: {scrubbed}"
                            );
                        }
                        let backoff = connect_backoff(attempts);
                        warn!("sendMessage connection error, retrying in {backoff:?} (attempt {attempts}/{MAX_SEND_RETRIES}): {scrubbed}");
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(self.scrub_err(e, "Failed to send message"));
                }
            };

            let status = resp.status();
            let resp_body: ApiResponse<serde_json::Value> = resp
                .json()
                .await
                .map_err(|e| self.scrub_err(e, "Failed to parse sendMessage response"))?;

            if resp_body.ok {
                self.circuit_breaker.record_success();
                return Ok(());
            }

            if status.as_u16() == 429 {
                let retry_secs = resp_body.retry_after_secs().unwrap_or(5);
                if retries_exhausted(&mut attempts) {
                    self.circuit_breaker.record_failure_status(429);
                    return Err(self.flood_error(
                        chat_id,
                        retry_secs,
                        format!("sendMessage rate limited after {MAX_SEND_RETRIES} retries"),
                    ));
                }
                if flood_should_fail_fast(retry_secs) {
                    // A long flood wait must fail closed, not sleep: this send
                    // is awaited inline on the polling loop, so sleeping here
                    // freezes every chat on the bot for the whole wait.
                    self.circuit_breaker.record_failure_status(429);
                    return Err(self.flood_error(
                        chat_id,
                        retry_secs,
                        format!(
                            "sendMessage flood-limited for {retry_secs}s — failing fast instead of blocking the send path"
                        ),
                    ));
                }
                let jitter_ms = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64)
                    % 1000;
                warn!("Telegram rate limited, retry after {retry_secs}s +{jitter_ms}ms jitter (attempt {attempts}/{MAX_SEND_RETRIES})");
                tokio::time::sleep(
                    Duration::from_secs(retry_secs) + Duration::from_millis(jitter_ms),
                )
                .await;
                continue;
            }

            self.circuit_breaker.record_failure_status(status.as_u16());
            let description = resp_body
                .description
                .unwrap_or_else(|| "unknown error".into());
            let message = format!("sendMessage failed ({}): {description}", status.as_u16());
            if is_markup_rejection(status.as_u16(), &description) {
                return Err(anyhow::Error::new(TelegramSendError::Markup(message)));
            }
            bail!(message);
        }
    }

    /// Send a "typing" chat action.
    /// Uses circuit breaker to prevent infinite 401 loops.
    pub async fn send_typing(&self, chat_id: i64) -> Result<()> {
        if self.circuit_breaker.is_open() {
            return Ok(());
        }

        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });

        let result = self
            .client
            .post(self.api_url("sendChatAction"))
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 401 {
                    self.circuit_breaker.record_failure_status(401);
                } else {
                    self.circuit_breaker.record_success();
                }
            }
            Err(_) => {
                // Network errors don't contribute to circuit breaker
            }
        }

        Ok(())
    }

    /// Send a voice message (OGG/Opus audio) to a chat.
    /// Uses Telegram's `sendVoice` API with multipart/form-data upload.
    pub async fn send_voice(
        &self,
        chat_id: i64,
        audio_bytes: &[u8],
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendVoice", || {
            let file_part = reqwest::multipart::Part::bytes(audio_bytes.to_vec())
                .file_name("voice.ogg")
                .mime_str("audio/ogg")?;

            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .part("voice", file_part);

            if let Some(cap) = caption {
                form = form.text("caption", cap.to_string());
            }
            if let Some(tid) = message_thread_id {
                form = form.text("message_thread_id", tid.to_string());
            }
            if let Some(rid) = reply_to_message_id {
                form = form.text("reply_to_message_id", rid.to_string());
            }
            Ok(form)
        })
        .await
    }

    /// Edit the text of an existing message via `editMessageText`.
    ///
    /// Used for streaming/draft UX — the agent sends an initial message, then
    /// progressively updates it as more output arrives. Telegram rejects edits
    /// to identical text with "message is not modified"; callers should compare
    /// before calling. Errors propagate; rate-limit retry not applied since
    /// edits are rarely batched and 429 is unusual here.
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let body = EditMessageTextRequest {
            chat_id,
            message_id,
            text: text.to_string(),
            parse_mode: parse_mode.map(String::from),
            reply_markup,
        };

        let resp_body: ApiResponse<serde_json::Value> = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call editMessageText"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse editMessageText response"))?;

        if !resp_body.ok {
            bail!(
                "editMessageText failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Delete a message via `deleteMessage`.
    ///
    /// Telegram limits: a bot can delete *its own* messages at any time. To
    /// delete *other users'* messages it must be a chat admin with the
    /// `can_delete_messages` right, and even then only within 48 hours in
    /// non-private chats. Errors propagate so callers can surface
    /// "permission denied" cases.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let body = DeleteMessageRequest {
            chat_id,
            message_id,
        };

        let resp_body: ApiResponse<serde_json::Value> = self
            .client
            .post(self.api_url("deleteMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call deleteMessage"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse deleteMessage response"))?;

        if !resp_body.ok {
            bail!(
                "deleteMessage failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Build a multipart form for a sendPhoto/sendVideo/etc. call.
    ///
    /// The `field` is the Telegram API field name (`photo`, `video`, …). For
    /// `MediaSource::FileId`/`Url`, the value is set as a text field — Telegram
    /// accepts this form. For `MediaSource::Bytes`, the file is attached.
    fn build_media_form(
        chat_id: i64,
        field: &str,
        source: &MediaSource<'_>,
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<reqwest::multipart::Form> {
        let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string());

        match source {
            MediaSource::FileId(id) => {
                form = form.text(field.to_string(), (*id).to_string());
            }
            MediaSource::Url(url) => {
                form = form.text(field.to_string(), (*url).to_string());
            }
            MediaSource::Bytes {
                bytes,
                filename,
                mime,
            } => {
                let mut part = reqwest::multipart::Part::bytes(bytes.to_vec())
                    .file_name((*filename).to_string());
                if let Some(m) = mime {
                    part = part
                        .mime_str(m)
                        .with_context(|| format!("Invalid mime type: {m}"))?;
                }
                form = form.part(field.to_string(), part);
            }
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }
        if let Some(tid) = message_thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }
        if let Some(rid) = reply_to_message_id {
            form = form.text("reply_to_message_id", rid.to_string());
        }

        Ok(form)
    }

    /// Internal: POST a multipart form to a Telegram media endpoint.
    ///
    /// Media uploads get their own per-request deadline (`MEDIA_SEND_TIMEOUT`):
    /// the client-wide 30s whole-request timeout aborts large uploads on slow
    /// uplinks. Retries mirror `send_single_message` — connection errors back
    /// off, 429 honors the body's `retry_after`, and the circuit breaker
    /// records outcomes. Multipart bodies are single-use, so `build_form`
    /// rebuilds the form for each attempt.
    async fn send_media(
        &self,
        method: &str,
        build_form: impl Fn() -> Result<reqwest::multipart::Form>,
    ) -> Result<()> {
        if self.circuit_breaker.is_open() {
            bail!("Telegram circuit breaker open, skipping {method}");
        }

        let mut attempts = 0u32;
        loop {
            let send_result = self
                .client
                .post(self.api_url(method))
                .timeout(MEDIA_SEND_TIMEOUT)
                .multipart(build_form()?)
                .send()
                .await;

            let resp = match send_result {
                Ok(r) => r,
                Err(e) => {
                    // Media transport retries are CONNECT-only, unlike the
                    // text lane's is_safe_to_retry: a timeout on a large
                    // multipart POST often fires after Telegram has already
                    // processed the upload (only the response was pending),
                    // and retrying would deliver the file twice. A connect
                    // failure provably never reached the server.
                    if e.is_connect() {
                        let scrubbed = self.scrub(&e.to_string());
                        if retries_exhausted(&mut attempts) {
                            // No breaker update: the circuit breaker is
                            // 401-scoped by design (auth-loop protection),
                            // not a network-outage detector.
                            bail!("{method} failed after {MAX_SEND_RETRIES} retries: {scrubbed}");
                        }
                        let backoff = connect_backoff(attempts);
                        warn!("{method} connection error, retrying in {backoff:?} (attempt {attempts}/{MAX_SEND_RETRIES}): {scrubbed}");
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(self.scrub_err(e, &format!("Failed to call {method}")));
                }
            };

            let status = resp.status();
            let resp_body: ApiResponse<serde_json::Value> = resp
                .json()
                .await
                .map_err(|e| self.scrub_err(e, &format!("Failed to parse {method} response")))?;

            if resp_body.ok {
                self.circuit_breaker.record_success();
                return Ok(());
            }

            if status.as_u16() == 429 {
                if retries_exhausted(&mut attempts) {
                    self.circuit_breaker.record_failure_status(429);
                    bail!("{method} rate limited after {MAX_SEND_RETRIES} retries");
                }
                let retry_secs = resp_body.retry_after_secs().unwrap_or(5);
                if flood_should_fail_fast(retry_secs) {
                    // Same fail-closed rule as the text lane: a long flood
                    // wait slept inline would pin the whole send path.
                    self.circuit_breaker.record_failure_status(429);
                    bail!(
                        "{method} flood-limited for {retry_secs}s — failing fast instead of blocking the send path"
                    );
                }
                warn!("Telegram rate limited on {method}, retry after {retry_secs}s (attempt {attempts}/{MAX_SEND_RETRIES})");
                tokio::time::sleep(Duration::from_secs(retry_secs)).await;
                continue;
            }

            self.circuit_breaker.record_failure_status(status.as_u16());
            bail!(
                "{method} failed ({}): {}",
                status.as_u16(),
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }

    /// Send a photo via `sendPhoto`. Caption max 1024 chars.
    pub async fn send_photo(
        &self,
        chat_id: i64,
        photo: MediaSource<'_>,
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendPhoto", || {
            Self::build_media_form(
                chat_id,
                "photo",
                &photo,
                caption,
                message_thread_id,
                reply_to_message_id,
            )
        })
        .await
    }

    /// Send a video via `sendVideo`.
    pub async fn send_video(
        &self,
        chat_id: i64,
        video: MediaSource<'_>,
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendVideo", || {
            Self::build_media_form(
                chat_id,
                "video",
                &video,
                caption,
                message_thread_id,
                reply_to_message_id,
            )
        })
        .await
    }

    /// Send a document (any file type) via `sendDocument`.
    pub async fn send_document(
        &self,
        chat_id: i64,
        document: MediaSource<'_>,
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendDocument", || {
            Self::build_media_form(
                chat_id,
                "document",
                &document,
                caption,
                message_thread_id,
                reply_to_message_id,
            )
        })
        .await
    }

    /// Send an animation (GIF/short video) via `sendAnimation`.
    pub async fn send_animation(
        &self,
        chat_id: i64,
        animation: MediaSource<'_>,
        caption: Option<&str>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendAnimation", || {
            Self::build_media_form(
                chat_id,
                "animation",
                &animation,
                caption,
                message_thread_id,
                reply_to_message_id,
            )
        })
        .await
    }

    /// Send a sticker via `sendSticker`. Stickers do not accept captions.
    pub async fn send_sticker(
        &self,
        chat_id: i64,
        sticker: MediaSource<'_>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        self.send_media("sendSticker", || {
            Self::build_media_form(
                chat_id,
                "sticker",
                &sticker,
                None,
                message_thread_id,
                reply_to_message_id,
            )
        })
        .await
    }

    /// Register a webhook URL with Telegram.
    pub async fn set_webhook(&self, url: &str, secret_token: Option<&str>) -> Result<()> {
        let mut body = serde_json::json!({ "url": url });
        if let Some(secret) = secret_token {
            body["secret_token"] = serde_json::json!(secret);
        }

        let resp: ApiResponse<bool> = self
            .client
            .post(self.api_url("setWebhook"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call setWebhook"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse setWebhook response"))?;

        if !resp.ok {
            bail!(
                "setWebhook failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Remove the webhook.
    pub async fn delete_webhook(&self) -> Result<()> {
        let resp: ApiResponse<bool> = self
            .client
            .post(self.api_url("deleteWebhook"))
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call deleteWebhook"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse deleteWebhook response"))?;

        if !resp.ok {
            bail!(
                "deleteWebhook failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Get file info by file_id (for future media download support).
    pub async fn get_file(&self, file_id: &str) -> Result<FileInfo> {
        let body = serde_json::json!({ "file_id": file_id });
        let resp: ApiResponse<FileInfo> = self
            .client
            .post(self.api_url("getFile"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call getFile"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse getFile response"))?;

        match resp.result {
            Some(info) if resp.ok => Ok(info),
            _ => bail!(
                "getFile failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            ),
        }
    }

    /// Download a file by its file_path (obtained from get_file).
    /// Limits download size to `TELEGRAM_MAX_FILE_SIZE_BYTES` to prevent memory exhaustion.
    pub async fn download_file(&self, file_path: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(self.file_url(file_path))
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to download file"))?;

        if let Some(len) = resp.content_length() {
            if len > TELEGRAM_MAX_FILE_SIZE_BYTES {
                bail!("File too large: {len} bytes (max {TELEGRAM_MAX_FILE_SIZE_BYTES})");
            }
        }

        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk. Inlined (rather
        // than the shared capped reader) because chunk-read errors embed the
        // request URL, which contains the bot token and must go through
        // `scrub_err`.
        let mut resp = resp;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to read file bytes"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > TELEGRAM_MAX_FILE_SIZE_BYTES {
                bail!(
                    "Downloaded file too large: exceeds max {TELEGRAM_MAX_FILE_SIZE_BYTES} bytes \
                     (Content-Length was absent or understated)"
                );
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }

    /// Send a message with an inline keyboard.
    pub async fn send_message_with_keyboard(
        &self,
        chat_id: i64,
        text: &str,
        keyboard: InlineKeyboardMarkup,
        parse_mode: Option<&str>,
        message_thread_id: Option<i64>,
    ) -> Result<()> {
        let body = SendMessageRequest {
            chat_id,
            text: text.to_string(),
            parse_mode: parse_mode.map(String::from),
            reply_to_message_id: None,
            message_thread_id,
            reply_markup: Some(keyboard),
            disable_notification: None,
        };

        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to send message with keyboard"))?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse sendMessage response"))?;

        if !resp_body.ok {
            bail!(
                "sendMessage (keyboard) failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Send a message silently (no notification sound).
    pub async fn send_message_silent(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        message_thread_id: Option<i64>,
    ) -> Result<()> {
        let body = SendMessageRequest {
            chat_id,
            text: text.to_string(),
            parse_mode: parse_mode.map(String::from),
            reply_to_message_id: None,
            message_thread_id,
            reply_markup: None,
            disable_notification: Some(true),
        };

        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to send silent message"))?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse sendMessage response"))?;

        if !resp_body.ok {
            bail!(
                "sendMessage (silent) failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Send a poll to a chat.
    pub async fn send_poll(
        &self,
        chat_id: i64,
        question: &str,
        options: &[String],
        is_anonymous: Option<bool>,
        message_thread_id: Option<i64>,
    ) -> Result<()> {
        if options.len() < 2 || options.len() > 10 {
            bail!("Poll must have 2-10 options, got {}", options.len());
        }

        let body = SendPollRequest {
            chat_id,
            question: question.to_string(),
            options: options.to_vec(),
            is_anonymous,
            message_thread_id,
        };

        let resp = self
            .client
            .post(self.api_url("sendPoll"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to send poll"))?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse sendPoll response"))?;

        if !resp_body.ok {
            bail!(
                "sendPoll failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }

    /// Set a reaction on a message.
    pub async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> Result<()> {
        let body = SetMessageReactionRequest {
            chat_id,
            message_id,
            reaction: vec![ReactionType::emoji(emoji)],
        };

        let result = self
            .client
            .post(self.api_url("setMessageReaction"))
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let resp_body: ApiResponse<serde_json::Value> = resp.json().await.map_err(|e| {
                    self.scrub_err(e, "Failed to parse setMessageReaction response")
                })?;

                if !resp_body.ok {
                    warn!(
                        "setMessageReaction failed: {}",
                        resp_body
                            .description
                            .unwrap_or_else(|| "unknown error".into())
                    );
                }
            }
            Err(e) => {
                warn!(
                    "setMessageReaction network error: {}",
                    self.scrub(&e.to_string())
                );
            }
        }

        Ok(())
    }

    /// Long-poll for updates (used when webhook is not configured).
    pub async fn get_updates(&self, offset: Option<i64>, timeout: u64) -> Result<Vec<Update>> {
        let mut body = serde_json::json!({
            "timeout": timeout,
            "allowed_updates": ["message", "edited_message", "callback_query"],
        });
        if let Some(off) = offset {
            body["offset"] = serde_json::json!(off);
        }

        let resp: ApiResponse<Vec<Update>> = self
            .client
            .post(self.api_url("getUpdates"))
            .json(&body)
            .timeout(Duration::from_secs(timeout + 10))
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call getUpdates"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse getUpdates response"))?;

        match resp.result {
            Some(updates) if resp.ok => Ok(updates),
            _ => bail!(
                "getUpdates failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            ),
        }
    }

    /// Register bot commands with Telegram's native menu via `setMyCommands`.
    pub async fn set_my_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let bot_commands: Vec<BotCommand> = commands
            .iter()
            .map(|(name, desc)| BotCommand {
                command: name.to_string(),
                description: desc.to_string(),
            })
            .collect();

        let body = serde_json::json!({ "commands": bot_commands });

        let resp: ApiResponse<bool> = self
            .client
            .post(self.api_url("setMyCommands"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to call setMyCommands"))?
            .json()
            .await
            .map_err(|e| self.scrub_err(e, "Failed to parse setMyCommands response"))?;

        if !resp.ok {
            bail!(
                "setMyCommands failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct BotCommand {
    command: String,
    description: String,
}

#[async_trait]
impl NativeCommandRegistration for TelegramClient {
    async fn register_commands(&self, commands: &[CommandDef]) -> Result<()> {
        let pairs: Vec<(&str, &str)> = commands.iter().map(|c| (c.name, c.description)).collect();
        self.set_my_commands(&pairs).await
    }
}

/// Determine if a network error is safe to retry (no data was sent/received).
fn is_safe_to_retry(err: &reqwest::Error) -> bool {
    err.is_connect() || (err.is_timeout() && err.status().is_none())
}

/// Advance the shared send-retry counter; `true` once the
/// `MAX_SEND_RETRIES` budget is spent and the caller must give up.
fn retries_exhausted(attempts: &mut u32) -> bool {
    *attempts += 1;
    *attempts > MAX_SEND_RETRIES
}

/// Exponential backoff for connection-error retries: 500ms on the first
/// attempt, doubling each subsequent attempt.
fn connect_backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt - 1))
}

/// A flood wait longer than `FLOOD_FAIL_FAST_SECS` must fail closed instead
/// of sleeping inline on the send path; a wait of exactly the threshold is
/// still ridden out.
fn flood_should_fail_fast(retry_secs: u64) -> bool {
    retry_secs > FLOOD_FAIL_FAST_SECS
}

/// Replace the bot token in error/log text with `[REDACTED]`.
///
/// Two passes: exact occurrences of the stored token, plus any
/// `/bot<segment>/`-shaped URL path (belt-and-braces for encodings where the
/// token is not byte-identical). Idempotent — already-redacted text passes
/// through unchanged.
fn redact_bot_token(text: &str, token: &str) -> String {
    let exact = if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[REDACTED]")
    };

    let mut out = String::with_capacity(exact.len());
    let mut rest = exact.as_str();
    while let Some(idx) = rest.find("/bot") {
        let (head, tail) = rest.split_at(idx + "/bot".len());
        out.push_str(head);
        // The token segment runs to the next path separator or delimiter.
        let end = tail
            .find(|c: char| c == '/' || c == ')' || c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(tail.len());
        if end == 0 || tail[..end] == *"[REDACTED]" {
            out.push_str(&tail[..end]);
        } else {
            out.push_str("[REDACTED]");
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.api_url("getMe"),
            "https://api.telegram.org/bot123:ABC/getMe"
        );
        assert_eq!(
            client.api_url("sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }

    #[test]
    fn send_voice_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.api_url("sendVoice"),
            "https://api.telegram.org/bot123:ABC/sendVoice"
        );
    }

    /// Regression: media sends used to bypass the 429-retry path entirely —
    /// a single rate-limit response failed the upload outright. The media
    /// lane must honor the body's `retry_after` and succeed on the retry,
    /// rebuilding the (single-use) multipart form for the second attempt.
    #[tokio::test]
    async fn media_send_retries_after_rate_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendPhoto"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Too Many Requests",
                "parameters": { "retry_after": 0 }
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendPhoto"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        client
            .send_photo(
                7,
                MediaSource::Bytes {
                    bytes: b"jpeg-bytes",
                    filename: "pic.jpg",
                    mime: Some("image/jpeg"),
                },
                Some("caption"),
                None,
                None,
            )
            .await
            .expect("photo send must succeed after the rate-limit retry");
    }

    /// A non-retryable API error must fail without exhausting retries.
    #[tokio::test]
    async fn media_send_surfaces_hard_api_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendDocument"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: file must be non-empty"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let err = client
            .send_document(
                7,
                MediaSource::Bytes {
                    bytes: b"",
                    filename: "empty.bin",
                    mime: None,
                },
                None,
                None,
                None,
            )
            .await
            .expect_err("400 must fail immediately");
        assert!(
            err.to_string().contains("file must be non-empty"),
            "API description must surface, got: {err}"
        );
    }

    /// A long flood wait must fail closed on the FIRST response — no sleep,
    /// no retry. Sends are awaited inline on the polling loop, so sleeping
    /// through a minutes-long `retry_after` freezes every chat on the bot
    /// (observed in production as a 97-minute pin).
    #[tokio::test]
    async fn send_message_fails_fast_on_long_flood_wait() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Too Many Requests: retry after 5820",
                "parameters": { "retry_after": 5820 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let started = std::time::Instant::now();
        let err = client
            .send_message(7, "hello", None, None, None)
            .await
            .expect_err("a long flood wait must fail, not sleep");
        assert!(
            err.to_string().contains("flood-limited for 5820s"),
            "error must name the wait so callers can log it, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(FLOOD_FAIL_FAST_SECS),
            "fail-fast must not sleep through the flood wait"
        );
    }

    /// After a flood refusal, sends to that chat make no request until the
    /// window ends (a call inside the window can extend the penalty). Other
    /// chats are not affected, and the chat opens again after the window.
    #[tokio::test]
    async fn flood_refusal_starts_a_cooldown_for_that_chat_only() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .and(body_string_contains("\"chat_id\":7,"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Too Many Requests: retry after 60",
                "parameters": { "retry_after": 60 }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let first = client
            .send_message(7, "one", None, None, None)
            .await
            .expect_err("flood refusal");
        assert!(matches!(
            first.downcast_ref::<TelegramSendError>(),
            Some(TelegramSendError::Flood {
                retry_after_secs: 60,
                ..
            })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // Same chat, through a clone: refused locally, no request.
        let second = client
            .clone()
            .send_message(7, "two", None, None, None)
            .await
            .expect_err("chat is in cooldown");
        match second.downcast_ref::<TelegramSendError>() {
            Some(TelegramSendError::Flood {
                retry_after_secs, ..
            }) => assert!(
                (1..=60).contains(retry_after_secs),
                "remaining wait, got {retry_after_secs}"
            ),
            other => panic!("expected Flood, got {other:?}"),
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // A different chat is not in cooldown.
        client
            .send_message(8, "three", None, None, None)
            .await
            .expect("other chat sends");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);

        // End of the window: the chat sends to the API again.
        client
            .flood_until
            .lock()
            .unwrap()
            .insert(7, Instant::now() - Duration::from_secs(1));
        assert!(client
            .send_message(7, "four", None, None, None)
            .await
            .is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    /// The cooldown never exceeds the cap, whatever wait Telegram names.
    #[test]
    fn flood_cooldown_is_capped() {
        let client = TelegramClient::with_base("123:ABC", "http://127.0.0.1:1").unwrap();
        let _err = client.flood_error(7, 86_400, "flood".into());
        let remaining = client.flood_cooldown_remaining(7).expect("cooldown set");
        assert!(
            remaining <= FLOOD_COOLDOWN_CAP.as_secs()
                && remaining > FLOOD_COOLDOWN_CAP.as_secs() - 5,
            "got {remaining}"
        );
        assert_eq!(client.flood_cooldown_remaining(8), None);
    }

    /// A chat that had a flood refusal and never sends again must not keep
    /// its entry for the life of the process.
    #[test]
    fn a_new_flood_entry_removes_the_expired_entries_of_other_chats() {
        let client = TelegramClient::with_base("123:ABC", "http://127.0.0.1:1").unwrap();
        {
            let mut map = client.flood_until.lock().unwrap();
            map.insert(1, Instant::now() - Duration::from_secs(1));
            map.insert(2, Instant::now() - Duration::from_secs(90));
            map.insert(3, Instant::now() + Duration::from_secs(60));
        }

        let _err = client.flood_error(4, 30, "flood".into());

        let mut chats: Vec<i64> = client.flood_until.lock().unwrap().keys().copied().collect();
        chats.sort_unstable();
        assert_eq!(
            chats,
            vec![3, 4],
            "expired entries go, active entries and the new entry stay"
        );
        assert!(client.flood_cooldown_remaining(3).is_some());
    }

    #[test]
    fn markup_rejection_classification() {
        let cases = [
            (400, "Bad Request: can't parse entities: Unclosed tag", true),
            (400, "Bad Request: CAN'T PARSE ENTITIES", true),
            (400, "Bad Request: chat not found", false),
            (429, "can't parse entities", false),
            (500, "can't parse entities", false),
        ];
        for (status, description, expected) in cases {
            assert_eq!(
                is_markup_rejection(status, description),
                expected,
                "{status} {description}"
            );
        }
    }

    /// A short flood wait (≤ the fail-fast threshold) is still worth riding
    /// out inline: sleep, retry, succeed.
    #[tokio::test]
    async fn send_message_retries_short_flood_wait() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Too Many Requests",
                "parameters": { "retry_after": 0 }
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        client
            .send_message(7, "hello", None, None, None)
            .await
            .expect("a short flood wait must retry and succeed");
    }

    /// The media lane applies the same fail-closed rule as the text lane.
    #[tokio::test]
    async fn media_send_fails_fast_on_long_flood_wait() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendPhoto"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Too Many Requests",
                "parameters": { "retry_after": 600 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let started = std::time::Instant::now();
        let err = client
            .send_photo(
                7,
                MediaSource::Bytes {
                    bytes: b"jpeg-bytes",
                    filename: "pic.jpg",
                    mime: Some("image/jpeg"),
                },
                None,
                None,
                None,
            )
            .await
            .expect_err("a long flood wait must fail, not sleep");
        assert!(
            err.to_string().contains("flood-limited for 600s"),
            "error must name the wait, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(FLOOD_FAIL_FAST_SECS),
            "fail-fast must not sleep through the flood wait"
        );
    }

    /// `send_typing` is fire-and-forget (always `Ok`), but it must actually
    /// POST sendChatAction and feed 401s into the circuit breaker — that
    /// breaker is what stops an invalid token from looping forever. After
    /// threshold consecutive 401s, the send path must refuse with the breaker
    /// open, not reach the network.
    #[tokio::test]
    async fn send_typing_401s_trip_breaker_and_block_sends() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let threshold = cortex::constants::TELEGRAM_CIRCUIT_FAILURE_THRESHOLD;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendChatAction"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Unauthorized"
            })))
            .expect(u64::from(threshold))
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        for _ in 0..threshold {
            client
                .send_typing(7)
                .await
                .expect("send_typing is fire-and-forget and must not error");
        }

        let err = client
            .send_message(7, "hello", None, None, None)
            .await
            .expect_err("sends must be refused once the breaker is open");
        assert!(
            err.to_string().contains("circuit breaker open"),
            "expected breaker refusal, got: {err}"
        );
    }

    /// A successful (non-401) sendChatAction must NOT count toward the
    /// breaker — typing indicators run constantly and would otherwise wedge
    /// the send path on a healthy bot.
    #[tokio::test]
    async fn send_typing_success_does_not_trip_breaker() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let threshold = cortex::constants::TELEGRAM_CIRCUIT_FAILURE_THRESHOLD;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        for _ in 0..threshold {
            client.send_typing(7).await.unwrap();
        }
        client
            .send_message(7, "hello", None, None, None)
            .await
            .expect("healthy typing indicators must leave the send path open");
    }

    /// `send_voice` must go over the wire and surface the API's failure
    /// description — a swallowed error would report success for a voice note
    /// that never arrived.
    #[tokio::test]
    async fn send_voice_surfaces_api_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendVoice"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: VOICE_MESSAGES_FORBIDDEN"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let err = client
            .send_voice(7, b"ogg-bytes", Some("caption"), None, None)
            .await
            .expect_err("an API failure must not report success");
        assert!(
            err.to_string().contains("VOICE_MESSAGES_FORBIDDEN"),
            "API description must surface, got: {err}"
        );
    }

    /// `send_voice` success path: the upload must actually hit sendVoice and
    /// return Ok on `ok: true`.
    #[tokio::test]
    async fn send_voice_posts_to_send_voice_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendVoice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        client
            .send_voice(7, b"ogg-bytes", None, None, None)
            .await
            .expect("voice send must succeed on ok response");
    }

    /// `edit_message_text` and `delete_message` must round-trip through their
    /// endpoints: `ok: true` succeeds, `ok: false` fails with the API's
    /// description. Both directions per method — an inverted or stubbed-out
    /// `ok` check would let callers believe an edit/delete happened
    /// (streaming drafts and cleanup both depend on the real outcome).
    #[tokio::test]
    async fn edit_and_delete_report_real_api_outcome() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cases = [
            ("editMessageText", "Bad Request: message is not modified"),
            ("deleteMessage", "Bad Request: message can't be deleted"),
        ];
        for (api_method, description) in cases {
            // Failure direction: ok=false must error with the description.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{api_method}")))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "ok": false,
                    "description": description
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            let err = match api_method {
                "editMessageText" => client
                    .edit_message_text(7, 100, "new text", None, None)
                    .await
                    .expect_err("ok=false must surface as an error"),
                _ => client
                    .delete_message(7, 100)
                    .await
                    .expect_err("ok=false must surface as an error"),
            };
            assert!(
                err.to_string().contains(description),
                "{api_method}: description must surface, got: {err}"
            );

            // Success direction: ok=true must return Ok.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{api_method}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": true,
                    "result": true
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            match api_method {
                "editMessageText" => client
                    .edit_message_text(7, 100, "new text", None, None)
                    .await
                    .expect("ok=true must succeed"),
                _ => client
                    .delete_message(7, 100)
                    .await
                    .expect("ok=true must succeed"),
            }
        }
    }

    /// `get_me` must trust the API's `ok` flag, not just the presence of a
    /// `result` payload: `ok=true` with a user succeeds, `ok=false` must fail
    /// with the description even when a `result` object is attached.
    #[tokio::test]
    async fn get_me_honors_the_ok_flag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // ok=true → the user comes through.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:ABC/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 42, "first_name": "Borg", "username": "borg_bot", "is_bot": true }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let user = client.get_me().await.expect("ok=true must succeed");
        assert_eq!(user.id, 42);
        assert_eq!(user.username.as_deref(), Some("borg_bot"));

        // ok=false with a result payload attached → still an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:ABC/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "result": { "id": 0, "first_name": "ghost" },
                "description": "Unauthorized"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let err = client.get_me().await.expect_err("ok=false must fail");
        assert!(
            err.to_string().contains("Unauthorized"),
            "description must surface, got: {err}"
        );
    }

    /// Only the FIRST chunk of a long message may carry
    /// `reply_to_message_id` — later chunks replying too would spam the
    /// original message with N reply notifications.
    #[tokio::test]
    async fn only_first_chunk_replies_to_the_original_message() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(2)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let text = "a".repeat(DEFAULT_MESSAGE_CHUNK_SIZE + 1);
        client
            .send_message(7, &text, None, None, Some(10))
            .await
            .expect("chunked send must succeed");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "expected exactly two chunks");
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            first["reply_to_message_id"], 10,
            "first chunk must reply to the original"
        );
        assert!(
            second.get("reply_to_message_id").is_none(),
            "later chunks must not reply: {second}"
        );
    }

    /// The retry budget spends exactly `MAX_SEND_RETRIES` attempts: the
    /// counter advances by one per call and reports exhausted only once the
    /// budget is crossed.
    #[test]
    fn retry_budget_spends_exactly_max_send_retries() {
        let mut attempts = 0u32;
        for expected in 1..=MAX_SEND_RETRIES {
            assert!(
                !retries_exhausted(&mut attempts),
                "attempt {expected} is within budget"
            );
            assert_eq!(attempts, expected, "counter must advance by one");
        }
        assert!(
            retries_exhausted(&mut attempts),
            "attempt {} exceeds the budget",
            MAX_SEND_RETRIES + 1
        );
        assert_eq!(attempts, MAX_SEND_RETRIES + 1);
    }

    /// Connection-error backoff doubles from 500ms per attempt.
    #[test]
    fn connect_backoff_doubles_from_500ms() {
        let cases = [(1, 500), (2, 1000), (3, 2000), (4, 4000), (5, 8000)];
        for (attempt, ms) in cases {
            assert_eq!(
                connect_backoff(attempt),
                Duration::from_millis(ms),
                "attempt {attempt}"
            );
        }
    }

    /// A flood wait of exactly the threshold is ridden out; one second more
    /// fails fast.
    #[test]
    fn flood_fail_fast_boundary() {
        assert!(!flood_should_fail_fast(0));
        assert!(!flood_should_fail_fast(FLOOD_FAIL_FAST_SECS));
        assert!(flood_should_fail_fast(FLOOD_FAIL_FAST_SECS + 1));
    }

    /// Persistent 429s must consume the full retry budget — exactly
    /// `MAX_SEND_RETRIES + 1` requests — then fail naming the budget. Both
    /// the text and media lanes share this contract.
    #[tokio::test]
    async fn rate_limit_retries_exhaust_after_budget_on_both_lanes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for lane in ["sendMessage", "sendPhoto"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{lane}")))
                .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                    "ok": false,
                    "description": "Too Many Requests",
                    "parameters": { "retry_after": 0 }
                })))
                .expect(u64::from(MAX_SEND_RETRIES) + 1)
                .mount(&server)
                .await;

            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            let err = match lane {
                "sendMessage" => client
                    .send_message(7, "hello", None, None, None)
                    .await
                    .expect_err("persistent 429 must exhaust retries"),
                _ => client
                    .send_photo(
                        7,
                        MediaSource::Bytes {
                            bytes: b"jpeg-bytes",
                            filename: "pic.jpg",
                            mime: Some("image/jpeg"),
                        },
                        None,
                        None,
                        None,
                    )
                    .await
                    .expect_err("persistent 429 must exhaust retries"),
            };
            assert!(
                err.to_string()
                    .contains(&format!("rate limited after {MAX_SEND_RETRIES} retries")),
                "{lane}: error must name the budget, got: {err}"
            );
        }
    }

    #[test]
    fn file_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.file_url("photos/file_1.jpg"),
            "https://api.telegram.org/file/bot123:ABC/photos/file_1.jpg"
        );
    }

    /// Regression: transport-error strings embed the request URL, whose path
    /// contains the bot token. Anything reaching `tracing::warn!` or
    /// `health.record_error` via the api boundary must come out scrubbed.
    #[test]
    fn error_text_containing_token_is_redacted() {
        let token = "123456789:AAF-secret_tokenXYZ";
        let cases = [
            // reqwest-style Display with the URL embedded
            format!(
                "error sending request for url (https://api.telegram.org/bot{token}/sendMessage): connection refused"
            ),
            // file download URL variant
            format!("https://api.telegram.org/file/bot{token}/photos/file_1.jpg: timed out"),
            // bare token outside a URL
            format!("unexpected token {token} in response"),
        ];
        for input in cases {
            let out = redact_bot_token(&input, token);
            assert!(
                !out.contains(token),
                "token must not survive redaction: {out}"
            );
            assert!(out.contains("[REDACTED]"), "expected marker in: {out}");
        }
    }

    /// The `/bot<segment>/` URL shape is masked even when the embedded token
    /// differs from the stored one (e.g. encoding drift), and redaction is
    /// idempotent.
    #[test]
    fn bot_url_shape_redacted_without_exact_token_match() {
        let input = "GET https://api.telegram.org/bot999:OTHER-token/getMe failed";
        let once = redact_bot_token(input, "123:ABC");
        assert!(!once.contains("999:OTHER-token"), "shape miss: {once}");
        assert_eq!(redact_bot_token(&once, "123:ABC"), once, "not idempotent");
        // Non-URL text without token material passes through untouched.
        assert_eq!(
            redact_bot_token("plain connection reset", "123:ABC"),
            "plain connection reset"
        );
    }

    #[test]
    fn scrub_err_flattens_and_redacts() {
        let client = TelegramClient::new("123:ABC").unwrap();
        // scrub() is the same path scrub_err feeds through; assert the client
        // wiring uses its own stored token.
        let scrubbed =
            client.scrub("request to https://api.telegram.org/bot123:ABC/sendMessage failed");
        assert!(!scrubbed.contains("123:ABC"), "leak: {scrubbed}");
        assert!(scrubbed.contains("/bot[REDACTED]/"), "marker: {scrubbed}");
    }

    #[test]
    fn send_message_request_serialization() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "hello".into(),
            parse_mode: Some("Markdown".into()),
            reply_to_message_id: None,
            message_thread_id: None,
            reply_markup: None,
            disable_notification: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["text"], "hello");
        assert_eq!(json["parse_mode"], "Markdown");
        assert!(json.get("reply_to_message_id").is_none());
        assert!(json.get("message_thread_id").is_none());
    }

    #[test]
    fn send_message_request_with_thread_and_reply() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "hello".into(),
            parse_mode: None,
            reply_to_message_id: Some(10),
            message_thread_id: Some(99),
            reply_markup: None,
            disable_notification: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["reply_to_message_id"], 10);
        assert_eq!(json["message_thread_id"], 99);
    }

    #[test]
    fn send_message_request_no_parse_mode() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "hello".into(),
            parse_mode: None,
            reply_to_message_id: None,
            message_thread_id: None,
            reply_markup: None,
            disable_notification: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("parse_mode").is_none());
    }

    #[test]
    fn send_poll_request_serialization() {
        let req = SendPollRequest {
            chat_id: 42,
            question: "Favorite?".into(),
            options: vec!["A".into(), "B".into()],
            is_anonymous: Some(false),
            message_thread_id: Some(5),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["question"], "Favorite?");
        assert_eq!(json["is_anonymous"], false);
        assert_eq!(json["message_thread_id"], 5);
    }

    #[test]
    fn set_message_reaction_serialization() {
        let req = SetMessageReactionRequest {
            chat_id: 42,
            message_id: 100,
            reaction: vec![ReactionType::emoji("👍")],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["message_id"], 100);
        assert_eq!(json["reaction"][0]["emoji"], "👍");
    }

    #[test]
    fn edit_message_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.api_url("editMessageText"),
            "https://api.telegram.org/bot123:ABC/editMessageText"
        );
    }

    #[test]
    fn delete_message_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.api_url("deleteMessage"),
            "https://api.telegram.org/bot123:ABC/deleteMessage"
        );
    }

    #[test]
    fn media_endpoints_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        for method in [
            "sendPhoto",
            "sendVideo",
            "sendDocument",
            "sendAnimation",
            "sendSticker",
        ] {
            assert_eq!(
                client.api_url(method),
                format!("https://api.telegram.org/bot123:ABC/{method}")
            );
        }
    }

    #[test]
    fn build_media_form_accepts_file_id_and_url_and_bytes() {
        // FileId variant
        let f = TelegramClient::build_media_form(
            42,
            "photo",
            &MediaSource::FileId("AgACAgIAAxkB"),
            None,
            None,
            None,
        );
        assert!(f.is_ok(), "file_id variant builds");

        // URL variant
        let f = TelegramClient::build_media_form(
            42,
            "video",
            &MediaSource::Url("https://example.com/v.mp4"),
            Some("a caption"),
            Some(99),
            Some(7),
        );
        assert!(
            f.is_ok(),
            "url variant builds with caption + thread + reply"
        );

        // Bytes variant with valid mime
        let bytes = vec![0u8, 1, 2, 3];
        let f = TelegramClient::build_media_form(
            42,
            "document",
            &MediaSource::Bytes {
                bytes: &bytes,
                filename: "x.bin",
                mime: Some("application/octet-stream"),
            },
            None,
            None,
            None,
        );
        assert!(f.is_ok(), "bytes variant with valid mime builds");

        // Bytes variant with no mime (Telegram applies default)
        let f = TelegramClient::build_media_form(
            42,
            "document",
            &MediaSource::Bytes {
                bytes: &bytes,
                filename: "x.bin",
                mime: None,
            },
            None,
            None,
            None,
        );
        assert!(f.is_ok(), "bytes variant without mime builds");
    }

    #[test]
    fn build_media_form_rejects_invalid_mime() {
        let bytes = vec![0u8];
        let result = TelegramClient::build_media_form(
            42,
            "document",
            &MediaSource::Bytes {
                bytes: &bytes,
                filename: "x.bin",
                mime: Some("not a mime"),
            },
            None,
            None,
            None,
        );
        assert!(
            result.is_err(),
            "invalid mime must surface as an error, not a silent default"
        );
    }

    /// `is_safe_to_retry` classifies real transport errors: a connect
    /// failure never reached the server (retry), a whole-request timeout
    /// with no status received is equally unacknowledged (retry), while a
    /// request that failed for any other reason must NOT be replayed —
    /// blind retries of acknowledged sends deliver duplicates.
    #[tokio::test]
    async fn is_safe_to_retry_classifies_real_transport_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Connect failure: bind a port, then drop the listener before dialing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = Client::new();
        let connect_err = client
            .get(format!("http://127.0.0.1:{closed_port}/"))
            .send()
            .await
            .expect_err("closed port must refuse the connection");
        assert!(connect_err.is_connect(), "precondition: a connect error");
        assert!(
            is_safe_to_retry(&connect_err),
            "a connect failure never reached the server and is safe to retry"
        );

        // Timeout with no status: the server stalls past the client deadline.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let impatient = Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let timeout_err = impatient
            .get(format!("{}/slow", server.uri()))
            .send()
            .await
            .expect_err("50ms deadline must expire");
        assert!(timeout_err.is_timeout(), "precondition: a timeout error");
        assert!(timeout_err.status().is_none(), "precondition: no status");
        assert!(
            is_safe_to_retry(&timeout_err),
            "a pre-response timeout is safe to retry"
        );

        // Neither connect nor timeout (a request-build failure): not retryable.
        let build_err = client
            .get("http://[invalid-host/")
            .send()
            .await
            .expect_err("malformed URL must fail");
        assert!(
            !build_err.is_connect() && !build_err.is_timeout(),
            "precondition: neither connect nor timeout"
        );
        assert!(
            !is_safe_to_retry(&build_err),
            "only connect/pre-response-timeout errors may be replayed"
        );
    }

    /// Redaction must stop at the token segment's terminating delimiter —
    /// the text after `/bot<token>` (method path, status text) is the part
    /// operators need to debug with and must survive verbatim.
    #[test]
    fn redaction_preserves_text_after_the_token_segment() {
        let cases = [
            (
                "url (https://api.telegram.org/bot123:ABC/sendMessage): refused",
                "url (https://api.telegram.org/bot[REDACTED]/sendMessage): refused",
            ),
            (
                "call (https://api.telegram.org/bot999:OTHER) timed out",
                "call (https://api.telegram.org/bot[REDACTED]) timed out",
            ),
            (
                "request to \"https://x.test/bot999:OTHER\" failed",
                "request to \"https://x.test/bot[REDACTED]\" failed",
            ),
            (
                "request to 'https://x.test/bot999:OTHER' failed",
                "request to 'https://x.test/bot[REDACTED]' failed",
            ),
            (
                "GET https://x.test/bot999:OTHER failed fast",
                "GET https://x.test/bot[REDACTED] failed fast",
            ),
            // An empty token segment (`/bot/`) has nothing to hide; the
            // text must pass through unchanged rather than gaining markers.
            ("path /bot/ handled", "path /bot/ handled"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_bot_token(input, "123:ABC"), expected);
        }
    }

    /// setWebhook / deleteWebhook / setMyCommands must round-trip the API's
    /// `ok` flag: `ok=true` succeeds, `ok=false` errors with the description.
    /// A stubbed-out or inverted check would report webhook/command
    /// registration success that never happened.
    #[tokio::test]
    async fn webhook_and_command_registration_honor_the_ok_flag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let call = |client: TelegramClient, api_method: &'static str| async move {
            match api_method {
                "setWebhook" => {
                    client
                        .set_webhook("https://example.com/hook", Some("s3cret"))
                        .await
                }
                "deleteWebhook" => client.delete_webhook().await,
                _ => client.set_my_commands(&[("start", "Start the bot")]).await,
            }
        };

        for api_method in ["setWebhook", "deleteWebhook", "setMyCommands"] {
            // Failure direction: ok=false must surface the description.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{api_method}")))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "ok": false,
                    "description": "Bad Request: bad url"
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            let err = call(client, api_method)
                .await
                .expect_err("ok=false must surface as an error");
            assert!(
                err.to_string().contains("bad url"),
                "{api_method}: description must surface, got: {err}"
            );

            // Success direction: ok=true must return Ok.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{api_method}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": true,
                    "result": true
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            call(client, api_method)
                .await
                .expect("ok=true must succeed");

            // The one request carried the real payload, not an empty body
            // (deleteWebhook takes no parameters, so it has none to check).
            if api_method != "deleteWebhook" {
                let requests = server.received_requests().await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
                match api_method {
                    "setWebhook" => {
                        assert_eq!(body["url"], "https://example.com/hook");
                        assert_eq!(body["secret_token"], "s3cret");
                    }
                    _ => {
                        assert_eq!(body["commands"][0]["command"], "start");
                        assert_eq!(body["commands"][0]["description"], "Start the bot");
                    }
                }
            }
        }
    }

    /// Every media variant must actually POST its endpoint and surface the
    /// API's failure description — a swallowed error would report success
    /// for media that never arrived.
    #[tokio::test]
    async fn media_variants_post_their_endpoints_and_surface_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for api_method in ["sendVideo", "sendAnimation", "sendSticker"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/bot123:ABC/{api_method}")))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "ok": false,
                    "description": "Bad Request: wrong file type"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
            let source = MediaSource::Bytes {
                bytes: b"media-bytes",
                filename: "clip.bin",
                mime: None,
            };
            let result = match api_method {
                "sendVideo" => client.send_video(7, source, None, None, None).await,
                "sendAnimation" => client.send_animation(7, source, None, None, None).await,
                _ => client.send_sticker(7, source, None, None).await,
            };
            let err = result.expect_err("an API failure must not report success");
            assert!(
                err.to_string().contains("wrong file type"),
                "{api_method}: description must surface, got: {err}"
            );
        }
    }

    /// `get_file` must trust the `ok` flag, not the mere presence of a
    /// `result` payload: ok=true yields the file info, ok=false errors even
    /// with a result attached.
    #[tokio::test]
    async fn get_file_honors_the_ok_flag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "F1",
                    "file_unique_id": "U1",
                    "file_size": 512,
                    "file_path": "photos/file_1.jpg"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let info = client.get_file("F1").await.expect("ok=true must succeed");
        assert_eq!(info.file_path.as_deref(), Some("photos/file_1.jpg"));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/getFile"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "result": { "file_id": "ghost", "file_unique_id": "ghost" },
                "description": "Bad Request: file is too big"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let err = client
            .get_file("F1")
            .await
            .expect_err("ok=false must fail even with a result attached");
        assert!(
            err.to_string().contains("file is too big"),
            "description must surface, got: {err}"
        );
    }

    /// `get_updates` must return the updates on ok=true and error with the
    /// description on ok=false — an always-bailing guard would stall the
    /// whole polling loop.
    #[tokio::test]
    async fn get_updates_honors_the_ok_flag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [{ "update_id": 41 }, { "update_id": 42 }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let updates = client
            .get_updates(Some(41), 0)
            .await
            .expect("ok=true must yield the updates");
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].update_id, 41);
        assert_eq!(updates[1].update_id, 42);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/getUpdates"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Conflict: terminated by other getUpdates request"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let err = client
            .get_updates(None, 0)
            .await
            .expect_err("ok=false must fail");
        assert!(
            err.to_string().contains("Conflict"),
            "description must surface, got: {err}"
        );
    }

    /// The `NativeCommandRegistration` impl must forward every command's
    /// name and description to setMyCommands — a no-op impl would leave the
    /// bot's native command menu empty.
    #[tokio::test]
    async fn register_commands_posts_the_command_defs() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/setMyCommands"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegramClient::with_base("123:ABC", &server.uri()).unwrap();
        let defs = &crate::commands::GATEWAY_COMMANDS[..2];
        client
            .register_commands(defs)
            .await
            .expect("registration must succeed on ok=true");

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let posted = body["commands"].as_array().expect("commands array");
        assert_eq!(posted.len(), defs.len());
        for (def, posted) in defs.iter().zip(posted) {
            assert_eq!(posted["command"], def.name);
            assert_eq!(posted["description"], def.description);
        }
    }

    #[test]
    fn chunking_integration() {
        const CHUNK: usize = crate::constants::DEFAULT_MESSAGE_CHUNK_SIZE;
        let long_text: String = "a".repeat(CHUNK * 2 + 500);
        let chunks = chunker::chunk_text(&long_text, CHUNK);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), CHUNK);
        assert_eq!(chunks[1].len(), CHUNK);
        assert_eq!(chunks[2].len(), 500);
    }
}
