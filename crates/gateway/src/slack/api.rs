use std::sync::Arc;

use anyhow::{bail, Context, Result};
use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{error, warn};

use super::echo::EchoCache;
use super::types::{
    AuthTestResponse, PinnedItem, PostMessageRequest, ReactionInfo, SlackChannelInfo, UserInfo,
};
use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT};
use crate::message_processing::chunker;
use crate::resilience::circuit_breaker::CircuitBreaker;
use crate::resilience::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

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
const MAX_FILE_DOWNLOAD: usize = cortex::constants::SLACK_MAX_FILE_SIZE;

/// Hostname prefix that `download_file` accepts in production. Slack file
/// `url_private` links always live under this host.
const SLACK_FILE_DOWNLOAD_PREFIX: &str = "https://files.slack.com/";

/// A client for the Slack Web API.
#[derive(Clone)]
pub struct SlackClient {
    client: Client,
    token: String,
    base_url: String,
    /// URL prefix that `download_file` is allowed to fetch from (SSRF guard).
    download_prefix: String,
    circuit_breaker: Arc<CircuitBreaker>,
    bot_user_id: Option<String>,
    echo_cache: Arc<Mutex<EchoCache>>,
}

impl SlackClient {
    pub fn new(token: &str) -> Result<Self> {
        Self::with_base(token, SLACK_API_BASE)
    }

    /// Construct against a non-default API base. Production always uses
    /// [`SLACK_API_BASE`]; tests point this at a local mock server. File
    /// downloads stay pinned to `files.slack.com` in production; under a
    /// custom base they are pinned to that base instead.
    pub(crate) fn with_base(token: &str, base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let download_prefix = if base_url == SLACK_API_BASE {
            SLACK_FILE_DOWNLOAD_PREFIX.to_string()
        } else {
            format!("{base_url}/")
        };
        Ok(Self {
            client: crate::resilience::http_retry::build_channel_client(
                "Slack",
                GATEWAY_HTTP_TIMEOUT,
                None,
            )?,
            token: token.to_string(),
            base_url,
            download_prefix,
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
            .post(format!("{}/auth.test", self.base_url))
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
        let chunks = chunker::chunks_for_delivery(text, DEFAULT_MESSAGE_CHUNK_SIZE)?;

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

        let policy = RateLimitPolicy::for_service("Slack chat.postMessage");

        let resp = send_with_rate_limit_retry(&policy, || async {
            self.client
                .post(format!("{}/chat.postMessage", self.base_url))
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
            .post(format!("{}/assistant.threads.setStatus", self.base_url))
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
            .post(format!("{}/reactions.add", self.base_url))
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
            .post(format!("{}/files.upload", self.base_url))
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
        if !url.starts_with(&self.download_prefix) {
            bail!("Refusing to download file from non-Slack domain");
        }

        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to download Slack file")?;

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(crate::constants::DEFAULT_ATTACHMENT_MIME)
            .to_string();

        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk while streaming.
        let bytes =
            cortex::media::read_body_capped(resp, MAX_FILE_DOWNLOAD as u64, "Slack file").await?;

        Ok((bytes, content_type))
    }

    /// Fetch recent messages from a Slack channel via `conversations.history`.
    pub async fn conversations_history(
        &self,
        channel: &str,
        limit: u32,
    ) -> Result<Vec<HistoryMessage>> {
        let resp = self
            .client
            .get(format!("{}/conversations.history", self.base_url))
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
            .post(format!("{}/reactions.remove", self.base_url))
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
            .get(format!("{}/conversations.replies", self.base_url))
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
            .get(format!("{}/reactions.get", self.base_url))
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
            .get(format!("{}/pins.list", self.base_url))
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
            .get(format!("{}/users.info", self.base_url))
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
            .get(format!("{}/conversations.list", self.base_url))
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
            .get(format!("{}/conversations.info", self.base_url))
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

    /// Open a modal view via `views.open`.
    ///
    /// `trigger_id` comes from a slash command, shortcut, or `block_actions`
    /// payload and is valid for ~3 seconds. `view` is the Block Kit modal
    /// definition (typically built with the `blocks::Block` DSL plus the
    /// surrounding `{type:"modal",title,...}` envelope).
    pub async fn views_open(&self, trigger_id: &str, view: serde_json::Value) -> Result<()> {
        let body = serde_json::json!({ "trigger_id": trigger_id, "view": view });
        let resp = self
            .slack_post("views.open", &body)
            .await
            .context("Failed to call views.open")?;
        self.check_slack_response("views.open", resp).await
    }

    /// Update an open modal in place via `views.update`.
    ///
    /// Either `view_id` or `external_id` identifies the target. We accept
    /// `view_id` since that's what `view_submission` payloads carry. The
    /// agent uses this to react to a modal submit by replacing the modal
    /// with a confirmation view rather than closing it.
    pub async fn views_update(&self, view_id: &str, view: serde_json::Value) -> Result<()> {
        let body = serde_json::json!({ "view_id": view_id, "view": view });
        let resp = self
            .slack_post("views.update", &body)
            .await
            .context("Failed to call views.update")?;
        self.check_slack_response("views.update", resp).await
    }

    /// Publish an App Home tab view via `views.publish`.
    ///
    /// Triggered from an `app_home_opened` event; renders the agent's home
    /// tab. `user_id` is the user whose Home tab to update.
    pub async fn views_publish(&self, user_id: &str, view: serde_json::Value) -> Result<()> {
        let body = serde_json::json!({ "user_id": user_id, "view": view });
        let resp = self
            .slack_post("views.publish", &body)
            .await
            .context("Failed to call views.publish")?;
        self.check_slack_response("views.publish", resp).await
    }

    /// POST a JSON body to a Slack API method.
    async fn slack_post(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/{method}", self.base_url);
        let policy = RateLimitPolicy::for_service("Slack");
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

    /// Check a Slack API response for ok/error.
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
        const CHUNK: usize = crate::constants::DEFAULT_MESSAGE_CHUNK_SIZE;
        let long_text: String = "a".repeat(CHUNK * 2 + 500);
        let chunks = chunker::chunk_text(&long_text, CHUNK);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), CHUNK);
        assert_eq!(chunks[1].len(), CHUNK);
        assert_eq!(chunks[2].len(), 500);
    }

    /// Slack returns string error codes; the retry layer keys off
    /// `is_fatal_slack_error` to decide whether the request is worth
    /// re-attempting. Misclassifying an auth/permission error as transient
    /// would burn retry budget on a guaranteed-failure call;
    /// misclassifying a `ratelimited` or transient 5xx as fatal would
    /// surface a permanent failure to the user when the channel could have
    /// recovered. Both directions must be exercised.
    #[test]
    fn is_fatal_slack_error_separates_permanent_from_retryable() {
        let fatal = [
            // Auth + token state — never retryable.
            "invalid_auth",
            "token_revoked",
            "account_inactive",
            "not_authed",
            "token_expired",
            // Permission / topology — won't change on retry.
            "missing_scope",
            "no_permission",
            "channel_not_found",
            "not_in_channel",
            "is_archived",
            // Pin-specific terminal states.
            "already_pinned",
            "not_pinned",
            "message_not_found",
        ];
        let retryable = [
            "ratelimited",
            "server_error",
            "service_unavailable",
            "timeout",
            "unknown_error",
            "",
        ];
        for code in fatal {
            assert!(
                is_fatal_slack_error(code),
                "{code:?} must classify as fatal (no retry)"
            );
        }
        for code in retryable {
            assert!(
                !is_fatal_slack_error(code),
                "{code:?} must classify as retryable"
            );
        }
    }

    /// Operator-facing hints for known fatal codes MUST contain the
    /// keyword the user is most likely to grep for when fixing the
    /// underlying problem (e.g. the env-var name for invalid_auth, the
    /// verb "invite" for not_in_channel). Unknown codes return an empty
    /// hint so callers can fall back to the raw code.
    #[test]
    fn slack_error_hint_targets_recovery_keyword_per_fatal_code() {
        let cases = [
            ("invalid_auth", "SLACK_BOT_TOKEN"),
            ("not_in_channel", "invite"),
            ("channel_not_found", "channel"),
            ("missing_scope", "scope"),
            ("is_archived", "archived"),
            // Token-lifecycle codes all point the operator at re-issuing
            // the revoked/expired token.
            ("account_inactive", "revoked"),
            ("token_revoked", "revoked"),
            ("token_expired", "revoked"),
        ];
        for (code, keyword) in cases {
            let hint = slack_error_hint(code);
            assert!(
                hint.contains(keyword),
                "hint for {code:?} must contain {keyword:?}, got {hint:?}"
            );
        }
        assert_eq!(slack_error_hint("ratelimited"), "");
        assert_eq!(slack_error_hint(""), "");
    }

    /// `views.open` requires both `trigger_id` and `view` and rejects
    /// the request if the view payload is flattened. Verifies the body
    /// shape survives the surrounding object construction without
    /// reshaping.
    #[test]
    fn views_open_body_carries_trigger_id_and_nested_view() {
        let view = serde_json::json!({"type": "modal", "title": {"type":"plain_text","text":"Hi"}});
        let body = serde_json::json!({"trigger_id": "tr1", "view": view });
        assert_eq!(body["trigger_id"], "tr1");
        assert_eq!(body["view"]["type"], "modal");
        assert_eq!(body["view"]["title"]["text"], "Hi");
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

    /// Wiremock-backed tests: every call must hit the right path with the
    /// bot token as a Bearer header, ship the documented payload shape,
    /// parse the documented response shape, and surface `ok:false` errors.
    mod http {
        use super::*;
        use wiremock::matchers::{body_json, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TOKEN: &str = "xoxb-test-token";

        fn client(server: &MockServer) -> SlackClient {
            SlackClient::with_base(TOKEN, &server.uri()).unwrap()
        }

        fn ok_body(extra: serde_json::Value) -> serde_json::Value {
            let mut body = serde_json::json!({ "ok": true });
            if let (Some(obj), Some(add)) = (body.as_object_mut(), extra.as_object()) {
                for (k, v) in add {
                    obj.insert(k.clone(), v.clone());
                }
            }
            body
        }

        /// `auth_test` must treat `ok:true` as success and persist the
        /// returned `user_id` so echo detection can compare against it.
        #[tokio::test]
        async fn auth_test_success_stores_bot_user_id() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/auth.test"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "user_id": "UBOT42", "user": "borg", "team": "acme" }),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let mut client = client(&server);
            let resp = client.auth_test().await.expect("ok:true must succeed");
            assert_eq!(resp.user.as_deref(), Some("borg"));
            assert_eq!(
                client.bot_user_id(),
                Some("UBOT42"),
                "auth_test must store the exact user_id from the response"
            );
        }

        /// `ok:false` from auth.test must fail with the error code and the
        /// operator hint — Slack keeps HTTP 200 for API-level failures.
        #[tokio::test]
        async fn auth_test_failure_surfaces_error_code_and_hint() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/auth.test"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "invalid_auth" }),
                ))
                .mount(&server)
                .await;

            let mut client = client(&server);
            let err = client.auth_test().await.expect_err("ok:false must fail");
            let msg = err.to_string();
            assert!(msg.contains("invalid_auth"), "got: {msg}");
            assert!(msg.contains("SLACK_BOT_TOKEN"), "got: {msg}");
            assert!(
                client.bot_user_id().is_none(),
                "a failed auth_test must not invent a bot user id"
            );
        }

        /// The accessor must hand back the one shared cache: a write made
        /// through it must be visible on the next call. (Echo filtering
        /// reads through this accessor while sends write the same cache.)
        #[tokio::test]
        async fn echo_cache_accessor_returns_the_shared_cache() {
            let server = MockServer::start().await;
            let client = client(&server);
            client.echo_cache().lock().await.remember("outbound text");
            assert!(
                client.echo_cache().lock().await.is_echo("outbound text"),
                "echo_cache() must return the same cache on every call"
            );
        }

        /// A successful post must send the documented chat.postMessage body
        /// once and record the text for echo detection.
        #[tokio::test]
        async fn post_message_sends_payload_and_records_echo() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat.postMessage"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(body_json(serde_json::json!({
                    "channel": "C123",
                    "text": "hello world",
                    "thread_ts": "111.222",
                })))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ok_body(serde_json::json!({}))),
                )
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            client
                .post_message("C123", "hello world", Some("111.222"))
                .await
                .expect("ok:true must succeed");
            assert!(
                client.echo_cache().lock().await.is_echo("hello world"),
                "sent text must land in the echo cache"
            );
        }

        /// An `ok:false` reply must surface the error code (with hint), and
        /// the text must NOT be recorded as an echo — the message never
        /// reached the channel.
        #[tokio::test]
        async fn post_message_surfaces_api_error_without_recording_echo() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat.postMessage"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "is_archived" }),
                ))
                .mount(&server)
                .await;

            let client = client(&server);
            let err = client
                .post_message("C123", "doomed", None)
                .await
                .expect_err("ok:false must fail");
            let msg = err.to_string();
            assert!(msg.contains("is_archived"), "got: {msg}");
            assert!(msg.contains("archived"), "hint must be attached: {msg}");
            assert!(
                !client.echo_cache().lock().await.is_echo("doomed"),
                "failed sends must not be remembered as echoes"
            );
        }

        /// With a thread_ts the status call must POST the documented
        /// payload exactly once; without one it must not touch the network
        /// (Slack's API requires a thread).
        #[tokio::test]
        async fn set_thread_status_posts_payload_only_when_threaded() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/assistant.threads.setStatus"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(body_json(serde_json::json!({
                    "channel_id": "C9",
                    "thread_ts": "42.007",
                    "status": "is thinking...",
                })))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ok_body(serde_json::json!({}))),
                )
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            client
                .set_thread_status("C9", None, "is thinking...")
                .await
                .expect("no-thread call is a no-op");
            client
                .set_thread_status("C9", Some("42.007"), "is thinking...")
                .await
                .expect("threaded call must succeed");
        }

        /// Reaction add/remove are fire-and-forget, but they must actually
        /// fire: one POST each to the right method with the emoji payload.
        #[tokio::test]
        async fn add_and_remove_reaction_post_the_emoji_payload() {
            let server = MockServer::start().await;
            let expected = serde_json::json!({
                "channel": "C1",
                "timestamp": "1.2",
                "name": "eyes",
            });
            for api in ["/reactions.add", "/reactions.remove"] {
                Mock::given(method("POST"))
                    .and(path(api))
                    .and(header("authorization", format!("Bearer {TOKEN}")))
                    .and(body_json(expected.clone()))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(ok_body(serde_json::json!({}))),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
            }

            let client = client(&server);
            client.add_reaction("C1", "1.2", "eyes").await;
            client.remove_reaction("C1", "1.2", "eyes").await;
        }

        /// files.upload success returns Ok; `ok:false` must surface the
        /// error code instead of pretending the file went out.
        #[tokio::test]
        async fn upload_file_reports_success_and_surfaces_api_error() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files.upload"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ok_body(serde_json::json!({}))),
                )
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/files.upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "invalid_auth" }),
                ))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            client
                .upload_file("C1", b"file-bytes", "notes.txt", Some("text"))
                .await
                .expect("ok:true upload must succeed");
            let err = client
                .upload_file("C1", b"file-bytes", "notes.txt", None)
                .await
                .expect_err("ok:false upload must fail");
            assert!(err.to_string().contains("invalid_auth"), "got: {err}");
        }

        /// A successful download must return the exact response bytes and
        /// the server's content-type, authenticated with the bot token.
        #[tokio::test]
        async fn download_file_returns_body_bytes_and_content_type() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/files-pri/T1-F1/photo.png"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(b"PNG-PAYLOAD".to_vec())
                        .insert_header("content-type", "image/png"),
                )
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let url = format!("{}/files-pri/T1-F1/photo.png", server.uri());
            let (bytes, content_type) = client
                .download_file(&url)
                .await
                .expect("allowed-domain download must succeed");
            assert_eq!(bytes, b"PNG-PAYLOAD");
            assert_eq!(content_type, "image/png");
        }

        /// The SSRF guard must refuse URLs outside the pinned file host
        /// before any request is made.
        #[tokio::test]
        async fn download_file_refuses_foreign_domain() {
            let server = MockServer::start().await;
            let client = client(&server);
            let err = client
                .download_file("https://127.0.0.1:9/etc/passwd")
                .await
                .expect_err("foreign domain must be refused");
            assert!(
                err.to_string().contains("non-Slack domain"),
                "must fail at the guard, not the request: {err}"
            );
        }

        /// conversations.history must pass channel+limit as query params
        /// and map the messages array into HistoryMessage rows.
        #[tokio::test]
        async fn conversations_history_sends_query_and_parses_messages() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/conversations.history"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(query_param("channel", "C7"))
                .and(query_param("limit", "25"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "messages": [
                        { "user": "U1", "text": "first", "ts": "1.1" },
                        { "user": "U2", "text": "second", "ts": "2.2" },
                        { "subtype": "bot_message", "text": "no user field" },
                    ]}),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let messages = client.conversations_history("C7", 25).await.unwrap();
            assert_eq!(messages.len(), 2, "rows without user/ts are skipped");
            assert_eq!(messages[0].user, "U1");
            assert_eq!(messages[0].text, "first");
            assert_eq!(messages[1].ts, "2.2");
        }

        #[tokio::test]
        async fn conversations_history_surfaces_api_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "channel_not_found" }),
                ))
                .mount(&server)
                .await;

            let client = client(&server);
            let err = client
                .conversations_history("C7", 10)
                .await
                .expect_err("ok:false must fail, never an empty history");
            assert!(err.to_string().contains("channel_not_found"), "got: {err}");
        }

        /// conversations.replies must pass channel+ts+limit and parse the
        /// thread messages.
        #[tokio::test]
        async fn fetch_thread_replies_sends_query_and_parses_messages() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/conversations.replies"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(query_param("channel", "C7"))
                .and(query_param("ts", "9.9"))
                .and(query_param("limit", "50"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "messages": [
                        { "user": "U1", "text": "parent", "ts": "9.9" },
                        { "user": "U2", "text": "reply", "ts": "10.1" },
                    ]}),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let messages = client.fetch_thread_replies("C7", "9.9", 50).await.unwrap();
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[1].text, "reply");
            assert_eq!(messages[1].user, "U2");
        }

        #[tokio::test]
        async fn fetch_thread_replies_surfaces_api_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/conversations.replies"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "thread_not_found" }),
                ))
                .mount(&server)
                .await;

            let client = client(&server);
            let err = client
                .fetch_thread_replies("C7", "9.9", 50)
                .await
                .expect_err("ok:false must fail, never an empty thread");
            assert!(err.to_string().contains("thread_not_found"), "got: {err}");
        }

        /// reactions.get must request the full reaction list for the target
        /// message and parse `/message/reactions`.
        #[tokio::test]
        async fn list_reactions_sends_query_and_parses_reactions() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/reactions.get"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(query_param("channel", "C7"))
                .and(query_param("timestamp", "3.3"))
                .and(query_param("full", "true"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "message": { "reactions": [
                        { "name": "eyes", "count": 2, "users": ["U1", "U2"] },
                    ]}}),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let reactions = client.list_reactions("C7", "3.3").await.unwrap();
            assert_eq!(reactions.len(), 1);
            assert_eq!(reactions[0].name, "eyes");
            assert_eq!(reactions[0].count, 2);
        }

        #[tokio::test]
        async fn list_reactions_surfaces_api_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/reactions.get"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "message_not_found" }),
                ))
                .mount(&server)
                .await;

            let client = client(&server);
            let err = client
                .list_reactions("C7", "3.3")
                .await
                .expect_err("ok:false must fail, never an empty list");
            assert!(err.to_string().contains("message_not_found"), "got: {err}");
        }

        /// edit/delete/pin/unpin all go through the shared JSON POST +
        /// ok-check path; each must send its payload and surface `ok:false`
        /// with the method name. Table-driven over the four methods.
        #[tokio::test]
        async fn mutating_calls_send_payload_and_surface_api_errors() {
            type Call = fn(
                SlackClient,
            )
                -> futures::future::BoxFuture<'static, Result<()>>;
            let cases: Vec<(&str, serde_json::Value, Call)> = vec![
                (
                    "chat.update",
                    serde_json::json!({ "channel": "C1", "ts": "5.5", "text": "edited" }),
                    |c| Box::pin(async move { c.edit_message("C1", "5.5", "edited").await }),
                ),
                (
                    "chat.delete",
                    serde_json::json!({ "channel": "C1", "ts": "5.5" }),
                    |c| Box::pin(async move { c.delete_message("C1", "5.5").await }),
                ),
                (
                    "pins.add",
                    serde_json::json!({ "channel": "C1", "timestamp": "5.5" }),
                    |c| Box::pin(async move { c.pin_message("C1", "5.5").await }),
                ),
                (
                    "pins.remove",
                    serde_json::json!({ "channel": "C1", "timestamp": "5.5" }),
                    |c| Box::pin(async move { c.unpin_message("C1", "5.5").await }),
                ),
            ];

            for (api, expected_body, call) in cases {
                // Success round: the payload must reach the endpoint.
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path(format!("/{api}")))
                    .and(header("authorization", format!("Bearer {TOKEN}")))
                    .and(body_json(expected_body.clone()))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(ok_body(serde_json::json!({}))),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                call(client(&server))
                    .await
                    .unwrap_or_else(|e| panic!("{api} ok:true must succeed: {e}"));

                // Error round: ok:false must fail and name the method.
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path(format!("/{api}")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({ "ok": false, "error": "some_error" }),
                    ))
                    .mount(&server)
                    .await;
                let err = call(client(&server))
                    .await
                    .expect_err("ok:false must fail");
                let msg = err.to_string();
                assert!(msg.contains(api), "error must name {api}: {msg}");
                assert!(msg.contains("some_error"), "got: {msg}");
            }
        }

        /// pins.list must query the channel and parse the items array.
        #[tokio::test]
        async fn list_pins_sends_query_and_parses_items() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/pins.list"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(query_param("channel", "C7"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "items": [
                        { "message": { "text": "pinned!", "user": "U1", "ts": "7.7" } },
                    ]}),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let items = client.list_pins("C7").await.unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(
                items[0].message.as_ref().unwrap().text.as_deref(),
                Some("pinned!")
            );
        }

        #[tokio::test]
        async fn list_pins_surfaces_api_error() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/pins.list"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "ok": false, "error": "channel_not_found" }),
                ))
                .mount(&server)
                .await;

            let client = client(&server);
            let err = client
                .list_pins("C7")
                .await
                .expect_err("ok:false must fail, never an empty pin list");
            assert!(err.to_string().contains("channel_not_found"), "got: {err}");
        }

        /// users.info on `ok:true` must parse the user object — the ok-check
        /// must not treat success as failure.
        #[tokio::test]
        async fn get_user_info_parses_user_on_success() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/users.info"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .and(query_param("user", "U55"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(
                    serde_json::json!({ "user": {
                        "id": "U55", "name": "alice", "is_bot": false,
                    }}),
                )))
                .expect(1)
                .mount(&server)
                .await;

            let client = client(&server);
            let user = client.get_user_info("U55").await.unwrap();
            assert_eq!(user.id, "U55");
            assert_eq!(user.name, "alice");
        }
    }
}
