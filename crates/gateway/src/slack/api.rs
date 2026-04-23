use std::sync::Arc;

use anyhow::{bail, Context, Result};
use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{error, warn};

use super::echo::EchoCache;
use super::types::{
    AuthTestResponse, PinnedItem, PostMessageRequest, ReactionInfo, SlackChannelInfo, UserInfo,
};
use crate::chunker;
use crate::circuit_breaker::CircuitBreaker;
use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT};
use crate::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

const SLACK_API_BASE: &str = crate::constants::SLACK_API_BASE;

/// Slack API error codes that will never succeed on retry — the caller must
/// intervene (fix credentials, invite the bot, unarchive the channel, etc.).
/// These are surfaced with a `FATAL` marker in error messages and logged at
/// `error!` level so operators notice them in production logs.
const FATAL_SLACK_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "missing_scope",
    "no_permission",
    "channel_not_found",
    "not_in_channel",
    "is_archived",
    "user_not_found",
    "team_added_to_org",
    "already_pinned",
    "not_pinned",
    "message_not_found",
];

/// Returns `true` if the given Slack API error code is non-recoverable.
pub(crate) fn is_fatal_slack_error(error: &str) -> bool {
    FATAL_SLACK_ERRORS.contains(&error)
}

/// Format a human-readable hint for a Slack API error code.
fn slack_error_hint(error: &str) -> &'static str {
    match error {
        "channel_not_found" => " — Borg may not be added to this channel",
        "not_in_channel" => " — invite the Borg to the channel first",
        "invalid_auth" | "not_authed" => " — check SLACK_BOT_TOKEN",
        "account_inactive" | "token_revoked" | "token_expired" => " — Borg token has been revoked",
        "missing_scope" | "no_permission" => " — Borg token is missing required OAuth scopes",
        "is_archived" => " — channel is archived",
        _ => "",
    }
}

/// Circuit breaker thresholds for Slack typing indicators.
/// Trips after N consecutive failures.
const TYPING_CB_FAILURE_THRESHOLD: u32 = crate::constants::SLACK_TYPING_CB_FAILURE_THRESHOLD;
const TYPING_CB_SUSPENSION_SECS: u64 = crate::constants::SLACK_TYPING_CB_SUSPENSION_SECS;

/// Max file download size for Slack attachments.
const MAX_FILE_DOWNLOAD: usize = borg_core::constants::SLACK_MAX_FILE_SIZE;

/// A client for the Slack Web API.
#[derive(Clone)]
pub struct SlackClient {
    client: Client,
    token: String,
    circuit_breaker: Arc<CircuitBreaker>,
    bot_user_id: Option<String>,
    echo_cache: Arc<Mutex<EchoCache>>,
}

impl SlackClient {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Slack HTTP client")?,
            token: token.to_string(),
            circuit_breaker: Arc::new(CircuitBreaker::new(
                TYPING_CB_FAILURE_THRESHOLD,
                TYPING_CB_SUSPENSION_SECS,
            )),
            bot_user_id: None,
            echo_cache: Arc::new(Mutex::new(EchoCache::new())),
        })
    }

    /// The bot's own user ID (set after `auth_test`).
    pub fn bot_user_id(&self) -> Option<&str> {
        self.bot_user_id.as_deref()
    }

    /// Reference to the echo cache for inbound filtering.
    pub fn echo_cache(&self) -> &Arc<Mutex<EchoCache>> {
        &self.echo_cache
    }

    /// Validate the bot token by calling `auth.test`.
    /// Stores the bot's user_id for echo detection.
    pub async fn auth_test(&mut self) -> Result<AuthTestResponse> {
        let resp: AuthTestResponse = self
            .client
            .post(format!("{SLACK_API_BASE}/auth.test"))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to call auth.test")?
            .json()
            .await
            .context("Failed to parse auth.test response")?;

        if !resp.ok {
            let err = resp.error.as_deref().unwrap_or("unknown error");
            if is_fatal_slack_error(err) {
                error!(
                    "Slack auth.test FATAL error: {err}{} — Borg cannot start until this is fixed",
                    slack_error_hint(err)
                );
            }
            bail!("auth.test failed: {err}{}", slack_error_hint(err));
        }

        self.bot_user_id = resp.user_id.clone();

        Ok(resp)
    }

    /// Send a message to a Slack channel, automatically chunking at 4000 chars.
    /// Optionally replies in a thread if `thread_ts` is provided.
    /// Records sent text in echo cache for self-message detection.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        let chunks = chunker::chunk_text_nonempty(text, DEFAULT_MESSAGE_CHUNK_SIZE);

        for chunk in &chunks {
            self.send_single_message(channel, chunk, thread_ts).await?;
        }

        // Record full text in echo cache after successful send
        self.echo_cache.lock().await.remember(text);
        Ok(())
    }

    async fn send_single_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        let body = PostMessageRequest {
            channel: channel.to_string(),
            text: text.to_string(),
            thread_ts: thread_ts.map(String::from),
            blocks: None,
        };

        let policy = RateLimitPolicy {
            service_name: "Slack chat.postMessage",
            ..RateLimitPolicy::default()
        };

        let resp = send_with_rate_limit_retry(&policy, || async {
            self.client
                .post(format!("{SLACK_API_BASE}/chat.postMessage"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .context("Failed to send Slack message")
        })
        .await?;

        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse chat.postMessage response")?;

        if resp_body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(());
        }

        let error = resp_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");

        let hint = slack_error_hint(error);

        if is_fatal_slack_error(error) {
            // Log at error level so operators notice credential/permission issues.
            // Prefix with FATAL so log scanners can alert on it.
            error!(
                "Slack chat.postMessage FATAL error ({}): {error}{hint} — this will not be retried",
                status.as_u16()
            );
            bail!(
                "chat.postMessage FATAL ({}): {error}{hint}",
                status.as_u16()
            );
        }

        bail!(
            "chat.postMessage failed ({}): {error}{hint}",
            status.as_u16()
        );
    }

    /// Set thread typing status via `assistant.threads.setStatus` API.
    /// Non-fatal: logs warning on failure. Requires `thread_ts`.
    pub async fn set_thread_status(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        status: &str,
    ) -> Result<()> {
        let Some(ts) = thread_ts else {
            return Ok(());
        };

        if self.circuit_breaker.is_open() {
            return Ok(());
        }

        let body = serde_json::json!({
            "channel_id": channel,
            "thread_ts": ts,
            "status": status,
        });

        let result = self
            .client
            .post(format!("{SLACK_API_BASE}/assistant.threads.setStatus"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let resp_body: serde_json::Value = resp
                    .json()
                    .await
                    .unwrap_or_else(|_| serde_json::json!({"ok": false}));

                if resp_body
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    self.circuit_breaker.record_success();
                } else {
                    self.circuit_breaker.record_failure();
                    let error = resp_body
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    warn!("assistant.threads.setStatus failed: {error}");
                }
            }
            Err(e) => {
                // Network errors don't trip circuit breaker
                warn!("assistant.threads.setStatus network error: {e}");
            }
        }

        Ok(())
    }

    /// Add a reaction emoji to a message. Non-fatal on failure.
    pub async fn add_reaction(&self, channel: &str, message_ts: &str, emoji: &str) {
        let body = serde_json::json!({
            "channel": channel,
            "timestamp": message_ts,
            "name": emoji,
        });

        let result = self
            .client
            .post(format!("{SLACK_API_BASE}/reactions.add"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await;

        if let Err(e) = result {
            warn!("reactions.add failed: {e}");
        }
    }

    /// Upload a file to a Slack channel via `files.upload`.
    pub async fn upload_file(
        &self,
        channels: &str,
        content: &[u8],
        filename: &str,
        filetype: Option<&str>,
    ) -> Result<()> {
        let mut form = reqwest::multipart::Form::new()
            .text("channels", channels.to_string())
            .text("filename", filename.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(content.to_vec()).file_name(filename.to_string()),
            );

        if let Some(ft) = filetype {
            form = form.text("filetype", ft.to_string());
        }

        let resp = self
            .client
            .post(format!("{SLACK_API_BASE}/files.upload"))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload file to Slack")?;

        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse files.upload response")?;

        if resp_body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(());
        }

        let error = resp_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");

        bail!("files.upload failed ({}): {}", status.as_u16(), error);
    }

    /// Download a file from Slack using `url_private` with Bearer auth.
    /// Returns the file bytes and content-type.
    /// Rejects files larger than the configured max size.
    /// Only downloads from `files.slack.com` to prevent SSRF.
    pub async fn download_file(&self, url: &str) -> Result<(Vec<u8>, String)> {
        // Validate URL domain to prevent SSRF
        if !url.starts_with("https://files.slack.com/") {
            bail!("Refusing to download file from non-Slack domain");
        }

        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to download Slack file")?;

        // Check Content-Length before buffering to prevent memory exhaustion
        if let Some(content_length) = resp.content_length() {
            if content_length as usize > MAX_FILE_DOWNLOAD {
                bail!("Slack file too large ({content_length} bytes, max {MAX_FILE_DOWNLOAD})");
            }
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Slack file bytes")?;

        if bytes.len() > MAX_FILE_DOWNLOAD {
            bail!(
                "Slack file too large ({} bytes, max {})",
                bytes.len(),
                MAX_FILE_DOWNLOAD
            );
        }

        Ok((bytes.to_vec(), content_type))
    }

    /// Fetch recent messages from a Slack channel via `conversations.history`.
    pub async fn conversations_history(
        &self,
        channel: &str,
        limit: u32,
    ) -> Result<Vec<HistoryMessage>> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/conversations.history"))
            .bearer_auth(&self.token)
            .query(&[("channel", channel), ("limit", &limit.to_string())])
            .send()
            .await
            .context("Failed to call conversations.history")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse conversations.history response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("conversations.history failed: {error}");
        }

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(HistoryMessage {
                            user: m.get("user")?.as_str()?.to_string(),
                            text: m.get("text")?.as_str()?.to_string(),
                            ts: m.get("ts")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(messages)
    }

    /// Remove a reaction emoji from a message. Non-fatal on failure.
    pub async fn remove_reaction(&self, channel: &str, message_ts: &str, emoji: &str) {
        let body = serde_json::json!({
            "channel": channel,
            "timestamp": message_ts,
            "name": emoji,
        });

        let result = self
            .client
            .post(format!("{SLACK_API_BASE}/reactions.remove"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await;

        if let Err(e) = result {
            warn!("reactions.remove failed: {e}");
        }
    }

    /// Edit an existing message via `chat.update`.
    pub async fn edit_message(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
            "text": text,
        });

        let resp = self
            .slack_post("chat.update", &body)
            .await
            .context("Failed to call chat.update")?;

        self.check_slack_response("chat.update", resp).await
    }

    /// Delete a message via `chat.delete`.
    pub async fn delete_message(&self, channel: &str, ts: &str) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
        });

        let resp = self
            .slack_post("chat.delete", &body)
            .await
            .context("Failed to call chat.delete")?;

        self.check_slack_response("chat.delete", resp).await
    }

    /// Fetch thread replies via `conversations.replies`.
    pub async fn fetch_thread_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u32,
    ) -> Result<Vec<HistoryMessage>> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/conversations.replies"))
            .bearer_auth(&self.token)
            .query(&[
                ("channel", channel),
                ("ts", thread_ts),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .context("Failed to call conversations.replies")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse conversations.replies response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("conversations.replies failed: {error}");
        }

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(HistoryMessage {
                            user: m.get("user")?.as_str()?.to_string(),
                            text: m.get("text")?.as_str()?.to_string(),
                            ts: m.get("ts")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(messages)
    }

    /// List reactions on a message via `reactions.get`.
    pub async fn list_reactions(&self, channel: &str, ts: &str) -> Result<Vec<ReactionInfo>> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/reactions.get"))
            .bearer_auth(&self.token)
            .query(&[("channel", channel), ("timestamp", ts), ("full", "true")])
            .send()
            .await
            .context("Failed to call reactions.get")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse reactions.get response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("reactions.get failed: {error}");
        }

        let reactions: Vec<ReactionInfo> = body
            .pointer("/message/reactions")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(reactions)
    }

    /// Pin a message via `pins.add`.
    pub async fn pin_message(&self, channel: &str, ts: &str) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "timestamp": ts,
        });

        let resp = self
            .slack_post("pins.add", &body)
            .await
            .context("Failed to call pins.add")?;

        self.check_slack_response("pins.add", resp).await
    }

    /// Unpin a message via `pins.remove`.
    pub async fn unpin_message(&self, channel: &str, ts: &str) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "timestamp": ts,
        });

        let resp = self
            .slack_post("pins.remove", &body)
            .await
            .context("Failed to call pins.remove")?;

        self.check_slack_response("pins.remove", resp).await
    }

    /// List pinned messages in a channel via `pins.list`.
    pub async fn list_pins(&self, channel: &str) -> Result<Vec<PinnedItem>> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/pins.list"))
            .bearer_auth(&self.token)
            .query(&[("channel", channel)])
            .send()
            .await
            .context("Failed to call pins.list")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse pins.list response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("pins.list failed: {error}");
        }

        let items: Vec<PinnedItem> = body
            .get("items")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(items)
    }

    /// Get user info via `users.info`.
    pub async fn get_user_info(&self, user_id: &str) -> Result<UserInfo> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/users.info"))
            .bearer_auth(&self.token)
            .query(&[("user", user_id)])
            .send()
            .await
            .context("Failed to call users.info")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse users.info response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let hint = slack_error_hint(error);
            bail!("users.info failed: {error}{hint}");
        }

        serde_json::from_value(
            body.get("user")
                .cloned()
                .context("users.info response missing 'user' field")?,
        )
        .context("Failed to parse user info")
    }

    /// List channels via `conversations.list`.
    pub async fn list_channels(&self, limit: u32) -> Result<Vec<SlackChannelInfo>> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/conversations.list"))
            .bearer_auth(&self.token)
            .query(&[
                ("limit", &limit.to_string()),
                ("types", &"public_channel,private_channel".to_string()),
            ])
            .send()
            .await
            .context("Failed to call conversations.list")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse conversations.list response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("conversations.list failed: {error}");
        }

        let channels: Vec<SlackChannelInfo> = body
            .get("channels")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(channels)
    }

    /// Get channel info via `conversations.info`.
    pub async fn get_channel_info(&self, channel: &str) -> Result<SlackChannelInfo> {
        let resp = self
            .client
            .get(format!("{SLACK_API_BASE}/conversations.info"))
            .bearer_auth(&self.token)
            .query(&[("channel", channel)])
            .send()
            .await
            .context("Failed to call conversations.info")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse conversations.info response")?;

        if !body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let hint = slack_error_hint(error);
            bail!("conversations.info failed: {error}{hint}");
        }

        serde_json::from_value(
            body.get("channel")
                .cloned()
                .context("conversations.info response missing 'channel' field")?,
        )
        .context("Failed to parse channel info")
    }

    /// Send an ephemeral message via `chat.postEphemeral`.
    pub async fn send_ephemeral(
        &self,
        channel: &str,
        user: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "channel": channel,
            "user": user,
            "text": text,
        });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let resp = self
            .slack_post("chat.postEphemeral", &body)
            .await
            .context("Failed to call chat.postEphemeral")?;

        self.check_slack_response("chat.postEphemeral", resp).await
    }

    /// Helper: POST a JSON body to a Slack API method.
    async fn slack_post(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{SLACK_API_BASE}/{method}");
        let policy = RateLimitPolicy {
            service_name: "Slack",
            ..RateLimitPolicy::default()
        };
        let client = &self.client;
        let token = &self.token;
        let body = body.clone();

        send_with_rate_limit_retry(&policy, || async {
            client
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .context("Slack API request failed")
        })
        .await
    }

    /// Helper: Check a Slack API response for ok/error.
    async fn check_slack_response(&self, method: &str, resp: reqwest::Response) -> Result<()> {
        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .context(format!("Failed to parse {method} response"))?;

        if resp_body
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(());
        }

        let error_str = resp_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");

        let hint = slack_error_hint(error_str);

        if is_fatal_slack_error(error_str) {
            error!(
                "Slack {method} FATAL error ({}): {error_str}{hint} — this will not be retried",
                status.as_u16()
            );
            bail!("{method} FATAL ({}): {error_str}{hint}", status.as_u16());
        }

        bail!("{method} failed ({}): {error_str}{hint}", status.as_u16());
    }
}

/// A message from `conversations.history` for channel context injection.
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub user: String,
    pub text: String,
    pub ts: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/auth.test"),
            "https://slack.com/api/auth.test"
        );
        assert_eq!(
            format!("{SLACK_API_BASE}/chat.postMessage"),
            "https://slack.com/api/chat.postMessage"
        );
    }

    #[test]
    fn post_message_request_serialization() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "hello".into(),
            thread_ts: Some("1234567890.111111".into()),
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["channel"], "C789");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["thread_ts"], "1234567890.111111");
    }

    #[test]
    fn post_message_request_no_thread() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "hello".into(),
            thread_ts: None,
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("thread_ts").is_none());
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

    #[test]
    fn set_thread_status_serializes_correct_json() {
        let body = serde_json::json!({
            "channel_id": "C123",
            "thread_ts": "1234567890.123456",
            "status": "is typing...",
        });
        assert_eq!(body["channel_id"], "C123");
        assert_eq!(body["thread_ts"], "1234567890.123456");
        assert_eq!(body["status"], "is typing...");
    }

    #[test]
    fn reaction_serializes_correct_json() {
        let body = serde_json::json!({
            "channel": "C123",
            "timestamp": "1234567890.123456",
            "name": "thinking_face",
        });
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["timestamp"], "1234567890.123456");
        assert_eq!(body["name"], "thinking_face");
    }

    #[test]
    fn circuit_breaker_initialized() {
        let client = SlackClient::new("xoxb-test").unwrap();
        assert!(!client.circuit_breaker.is_open());
    }

    #[test]
    fn file_upload_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/files.upload"),
            "https://slack.com/api/files.upload"
        );
    }

    #[test]
    fn is_fatal_slack_error_classifies_auth_errors() {
        assert!(is_fatal_slack_error("invalid_auth"));
        assert!(is_fatal_slack_error("token_revoked"));
        assert!(is_fatal_slack_error("account_inactive"));
        assert!(is_fatal_slack_error("not_authed"));
        assert!(is_fatal_slack_error("token_expired"));
    }

    #[test]
    fn is_fatal_slack_error_classifies_permission_errors() {
        assert!(is_fatal_slack_error("missing_scope"));
        assert!(is_fatal_slack_error("no_permission"));
        assert!(is_fatal_slack_error("channel_not_found"));
        assert!(is_fatal_slack_error("not_in_channel"));
        assert!(is_fatal_slack_error("is_archived"));
    }

    #[test]
    fn is_fatal_slack_error_excludes_transient_errors() {
        // These should NOT be fatal — they can succeed on retry.
        assert!(!is_fatal_slack_error("ratelimited"));
        assert!(!is_fatal_slack_error("server_error"));
        assert!(!is_fatal_slack_error("service_unavailable"));
        assert!(!is_fatal_slack_error("timeout"));
        assert!(!is_fatal_slack_error("unknown_error"));
        assert!(!is_fatal_slack_error(""));
    }

    #[test]
    fn slack_error_hint_present_for_known_errors() {
        assert!(slack_error_hint("invalid_auth").contains("SLACK_BOT_TOKEN"));
        assert!(slack_error_hint("not_in_channel").contains("invite"));
        assert!(slack_error_hint("channel_not_found").contains("channel"));
        assert!(slack_error_hint("missing_scope").contains("scope"));
        assert!(slack_error_hint("is_archived").contains("archived"));
    }

    #[test]
    fn slack_error_hint_empty_for_unknown() {
        assert_eq!(slack_error_hint("ratelimited"), "");
        assert_eq!(slack_error_hint(""), "");
    }

    // ── New API method tests ──

    #[test]
    fn edit_message_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/chat.update"),
            "https://slack.com/api/chat.update"
        );
    }

    #[test]
    fn delete_message_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/chat.delete"),
            "https://slack.com/api/chat.delete"
        );
    }

    #[test]
    fn fetch_thread_replies_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/conversations.replies"),
            "https://slack.com/api/conversations.replies"
        );
    }

    #[test]
    fn list_reactions_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/reactions.get"),
            "https://slack.com/api/reactions.get"
        );
    }

    #[test]
    fn pin_message_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/pins.add"),
            "https://slack.com/api/pins.add"
        );
    }

    #[test]
    fn unpin_message_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/pins.remove"),
            "https://slack.com/api/pins.remove"
        );
    }

    #[test]
    fn list_pins_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/pins.list"),
            "https://slack.com/api/pins.list"
        );
    }

    #[test]
    fn get_user_info_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/users.info"),
            "https://slack.com/api/users.info"
        );
    }

    #[test]
    fn list_channels_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/conversations.list"),
            "https://slack.com/api/conversations.list"
        );
    }

    #[test]
    fn get_channel_info_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/conversations.info"),
            "https://slack.com/api/conversations.info"
        );
    }

    #[test]
    fn send_ephemeral_url_construction() {
        assert_eq!(
            format!("{SLACK_API_BASE}/chat.postEphemeral"),
            "https://slack.com/api/chat.postEphemeral"
        );
    }

    #[test]
    fn edit_message_body_serialization() {
        let body = serde_json::json!({
            "channel": "C123",
            "ts": "1234567890.123456",
            "text": "updated text",
        });
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["ts"], "1234567890.123456");
        assert_eq!(body["text"], "updated text");
    }

    #[test]
    fn delete_message_body_serialization() {
        let body = serde_json::json!({
            "channel": "C123",
            "ts": "1234567890.123456",
        });
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["ts"], "1234567890.123456");
    }

    #[test]
    fn pin_message_body_serialization() {
        let body = serde_json::json!({
            "channel": "C123",
            "timestamp": "1234567890.123456",
        });
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["timestamp"], "1234567890.123456");
    }

    #[test]
    fn send_ephemeral_body_serialization() {
        let mut body = serde_json::json!({
            "channel": "C123",
            "user": "U456",
            "text": "only you can see this",
        });
        body["thread_ts"] = serde_json::Value::String("1234567890.123456".into());
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["user"], "U456");
        assert_eq!(body["text"], "only you can see this");
        assert_eq!(body["thread_ts"], "1234567890.123456");
    }

    #[test]
    fn send_ephemeral_body_no_thread() {
        let body = serde_json::json!({
            "channel": "C123",
            "user": "U456",
            "text": "ephemeral msg",
        });
        assert!(body.get("thread_ts").is_none());
    }

    #[test]
    fn is_fatal_slack_error_classifies_pin_errors() {
        assert!(is_fatal_slack_error("already_pinned"));
        assert!(is_fatal_slack_error("not_pinned"));
        assert!(is_fatal_slack_error("message_not_found"));
    }

    #[test]
    fn reactions_get_response_deserialization() {
        let json = r#"{
            "ok": true,
            "message": {
                "reactions": [
                    {"name": "thumbsup", "count": 2, "users": ["U1", "U2"]},
                    {"name": "heart", "count": 1, "users": ["U3"]}
                ]
            }
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let reactions: Vec<super::super::types::ReactionInfo> = body
            .pointer("/message/reactions")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0].name, "thumbsup");
        assert_eq!(reactions[0].count, 2);
        assert_eq!(reactions[1].name, "heart");
    }

    #[test]
    fn users_info_response_deserialization() {
        let json = r#"{
            "ok": true,
            "user": {
                "id": "U123",
                "name": "alice",
                "real_name": "Alice Smith",
                "is_bot": false
            }
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let user: super::super::types::UserInfo =
            serde_json::from_value(body["user"].clone()).unwrap();
        assert_eq!(user.id, "U123");
        assert_eq!(user.name, "alice");
        assert!(!user.is_bot);
    }

    #[test]
    fn conversations_list_response_deserialization() {
        let json = r#"{
            "ok": true,
            "channels": [
                {"id": "C1", "name": "general", "is_private": false},
                {"id": "G1", "name": "secret", "is_private": true, "topic": {"value": "Top secret"}}
            ]
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let channels: Vec<super::super::types::SlackChannelInfo> =
            serde_json::from_value(body["channels"].clone()).unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].id, "C1");
        assert!(!channels[0].is_private);
        assert!(channels[1].is_private);
        assert_eq!(channels[1].topic.as_ref().unwrap().value, "Top secret");
    }

    #[test]
    fn conversations_info_response_deserialization() {
        let json = r#"{
            "ok": true,
            "channel": {
                "id": "C123",
                "name": "general",
                "is_private": false,
                "topic": {"value": "Hello"}
            }
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let channel: super::super::types::SlackChannelInfo =
            serde_json::from_value(body["channel"].clone()).unwrap();
        assert_eq!(channel.id, "C123");
        assert_eq!(channel.name.as_deref(), Some("general"));
    }

    #[test]
    fn pins_list_response_deserialization() {
        let json = r#"{
            "ok": true,
            "items": [
                {
                    "message": {
                        "text": "Important!",
                        "user": "U123",
                        "ts": "1234567890.123456"
                    }
                },
                {}
            ]
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let items: Vec<super::super::types::PinnedItem> =
            serde_json::from_value(body["items"].clone()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].message.as_ref().unwrap().text.as_deref(),
            Some("Important!")
        );
        assert!(items[1].message.is_none());
    }

    #[test]
    fn conversations_replies_response_deserialization() {
        let json = r#"{
            "ok": true,
            "messages": [
                {"user": "U1", "text": "parent msg", "ts": "1234567890.111"},
                {"user": "U2", "text": "reply", "ts": "1234567890.222"}
            ]
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let messages: Vec<HistoryMessage> = body
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(HistoryMessage {
                            user: m.get("user")?.as_str()?.to_string(),
                            text: m.get("text")?.as_str()?.to_string(),
                            ts: m.get("ts")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "parent msg");
        assert_eq!(messages[1].user, "U2");
    }
}
