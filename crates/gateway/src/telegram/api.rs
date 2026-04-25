use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use tracing::warn;

use super::circuit_breaker::CircuitBreaker;
use super::types::{
    ApiResponse, DeleteMessageRequest, EditMessageTextRequest, FileInfo, InlineKeyboardMarkup,
    MediaSource, ReactionType, SendMessageRequest, SendPollRequest, SetMessageReactionRequest,
    Update, User,
};
use crate::chunker;
use crate::commands::{CommandDef, NativeCommandRegistration};
use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT};

const TELEGRAM_API_BASE: &str = crate::constants::TELEGRAM_API_BASE;
const HTTP_CONNECT_TIMEOUT: Duration = crate::constants::TELEGRAM_HTTP_CONNECT_TIMEOUT;

// Telegram returns retry_after in JSON body, not Retry-After header, so we use custom retry logic instead of http_retry.
const MAX_SEND_RETRIES: u32 = crate::constants::TELEGRAM_MAX_SEND_RETRIES;
const MAX_RETRY_AFTER_SECS: u64 = crate::constants::TELEGRAM_MAX_RETRY_AFTER_SECS;

/// A client for the Telegram Bot API.
#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    token: String,
    circuit_breaker: Arc<CircuitBreaker>,
}

impl TelegramClient {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Telegram HTTP client")?,
            token: token.to_string(),
            circuit_breaker: Arc::new(CircuitBreaker::new()),
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("{TELEGRAM_API_BASE}/bot{}/{method}", self.token)
    }

    fn file_url(&self, file_path: &str) -> String {
        format!("{TELEGRAM_API_BASE}/file/bot{}/{file_path}", self.token)
    }

    /// Validate the bot token by calling getMe.
    pub async fn get_me(&self) -> Result<User> {
        let resp: ApiResponse<User> = self
            .client
            .get(self.api_url("getMe"))
            .send()
            .await
            .context("Failed to call getMe")?
            .json()
            .await
            .context("Failed to parse getMe response")?;

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
        let chunks = chunker::chunk_text_nonempty(text, DEFAULT_MESSAGE_CHUNK_SIZE);

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
                        attempts += 1;
                        if attempts > MAX_SEND_RETRIES {
                            bail!("sendMessage failed after {MAX_SEND_RETRIES} retries: {e}");
                        }
                        let backoff = Duration::from_millis(500 * 2u64.pow(attempts - 1));
                        warn!("sendMessage connection error, retrying in {backoff:?} (attempt {attempts}/{MAX_SEND_RETRIES}): {e}");
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(e).context("Failed to send message");
                }
            };

            let status = resp.status();
            let resp_body: ApiResponse<serde_json::Value> = resp
                .json()
                .await
                .context("Failed to parse sendMessage response")?;

            if resp_body.ok {
                self.circuit_breaker.record_success();
                return Ok(());
            }

            // Handle 429 rate limiting
            if status.as_u16() == 429 {
                attempts += 1;
                if attempts > MAX_SEND_RETRIES {
                    self.circuit_breaker.record_failure(429);
                    bail!("sendMessage rate limited after {MAX_SEND_RETRIES} retries");
                }
                let retry_secs = resp_body.retry_after.unwrap_or(5).min(MAX_RETRY_AFTER_SECS);
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

            self.circuit_breaker.record_failure(status.as_u16());
            bail!(
                "sendMessage failed ({}): {}",
                status.as_u16(),
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
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
                    self.circuit_breaker.record_failure(401);
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

        let resp = self
            .client
            .post(self.api_url("sendVoice"))
            .multipart(form)
            .send()
            .await
            .context("Failed to send voice message")?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .context("Failed to parse sendVoice response")?;

        if !resp_body.ok {
            bail!(
                "sendVoice failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
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
            .context("Failed to call editMessageText")?
            .json()
            .await
            .context("Failed to parse editMessageText response")?;

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
            .context("Failed to call deleteMessage")?
            .json()
            .await
            .context("Failed to parse deleteMessage response")?;

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
    async fn send_media(&self, method: &str, form: reqwest::multipart::Form) -> Result<()> {
        let resp = self
            .client
            .post(self.api_url(method))
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("Failed to call {method}"))?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .with_context(|| format!("Failed to parse {method} response"))?;

        if !resp_body.ok {
            bail!(
                "{method} failed: {}",
                resp_body
                    .description
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
        Ok(())
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
        let form = Self::build_media_form(
            chat_id,
            "photo",
            &photo,
            caption,
            message_thread_id,
            reply_to_message_id,
        )?;
        self.send_media("sendPhoto", form).await
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
        let form = Self::build_media_form(
            chat_id,
            "video",
            &video,
            caption,
            message_thread_id,
            reply_to_message_id,
        )?;
        self.send_media("sendVideo", form).await
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
        let form = Self::build_media_form(
            chat_id,
            "document",
            &document,
            caption,
            message_thread_id,
            reply_to_message_id,
        )?;
        self.send_media("sendDocument", form).await
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
        let form = Self::build_media_form(
            chat_id,
            "animation",
            &animation,
            caption,
            message_thread_id,
            reply_to_message_id,
        )?;
        self.send_media("sendAnimation", form).await
    }

    /// Send a sticker via `sendSticker`. Stickers do not accept captions.
    pub async fn send_sticker(
        &self,
        chat_id: i64,
        sticker: MediaSource<'_>,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
    ) -> Result<()> {
        let form = Self::build_media_form(
            chat_id,
            "sticker",
            &sticker,
            None,
            message_thread_id,
            reply_to_message_id,
        )?;
        self.send_media("sendSticker", form).await
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
            .context("Failed to call setWebhook")?
            .json()
            .await
            .context("Failed to parse setWebhook response")?;

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
            .context("Failed to call deleteWebhook")?
            .json()
            .await
            .context("Failed to parse deleteWebhook response")?;

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
            .context("Failed to call getFile")?
            .json()
            .await
            .context("Failed to parse getFile response")?;

        match resp.result {
            Some(info) if resp.ok => Ok(info),
            _ => bail!(
                "getFile failed: {}",
                resp.description.unwrap_or_else(|| "unknown error".into())
            ),
        }
    }

    /// Download a file by its file_path (obtained from get_file).
    /// Limits download size to 25 MB to prevent memory exhaustion.
    pub async fn download_file(&self, file_path: &str) -> Result<Vec<u8>> {
        const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024; // 25 MB

        let resp = self
            .client
            .get(self.file_url(file_path))
            .send()
            .await
            .context("Failed to download file")?;

        if let Some(len) = resp.content_length() {
            if len > MAX_FILE_SIZE {
                bail!("File too large: {len} bytes (max {MAX_FILE_SIZE})");
            }
        }

        let bytes = resp.bytes().await.context("Failed to read file bytes")?;

        if bytes.len() as u64 > MAX_FILE_SIZE {
            bail!(
                "Downloaded file too large: {} bytes (max {MAX_FILE_SIZE})",
                bytes.len()
            );
        }

        Ok(bytes.to_vec())
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
            .context("Failed to send message with keyboard")?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .context("Failed to parse sendMessage response")?;

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
            .context("Failed to send silent message")?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .context("Failed to parse sendMessage response")?;

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
            .context("Failed to send poll")?;

        let resp_body: ApiResponse<serde_json::Value> = resp
            .json()
            .await
            .context("Failed to parse sendPoll response")?;

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
                let resp_body: ApiResponse<serde_json::Value> = resp
                    .json()
                    .await
                    .context("Failed to parse setMessageReaction response")?;

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
                warn!("setMessageReaction network error: {e}");
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
            .context("Failed to call getUpdates")?
            .json()
            .await
            .context("Failed to parse getUpdates response")?;

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
            .context("Failed to call setMyCommands")?
            .json()
            .await
            .context("Failed to parse setMyCommands response")?;

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

    #[test]
    fn file_url_construction() {
        let client = TelegramClient::new("123:ABC").unwrap();
        assert_eq!(
            client.file_url("photos/file_1.jpg"),
            "https://api.telegram.org/file/bot123:ABC/photos/file_1.jpg"
        );
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

    #[test]
    fn chunking_integration() {
        let long_text: String = "a".repeat(8500);
        let chunks = chunker::chunk_text(&long_text, 4000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 4000);
        assert_eq!(chunks[1].len(), 4000);
        assert_eq!(chunks[2].len(), 500);
    }
}
