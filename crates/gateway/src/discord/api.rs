use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, warn};

use super::types::{
    ChannelInfo, ChannelMessage, CreateMessageRequest, CurrentUser, InteractionResponse,
    MemberInfo, MessageQuery, RawChannelMessage, ReactionUser, ThreadInfo,
};
use crate::chunker;
use crate::commands::{CommandDef, NativeCommandRegistration};
use crate::constants::GATEWAY_HTTP_TIMEOUT;
use crate::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_MESSAGE_CHUNK_SIZE: usize = 2000;

/// Validate that a Discord snowflake ID is numeric (prevents path traversal in URL interpolation).
fn validate_snowflake(id: &str, label: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        bail!("Invalid Discord {label}: must be numeric, got {id:?}");
    }
    Ok(())
}

/// Validate that an emoji string is safe for URL interpolation (no path separators).
fn validate_emoji(emoji: &str) -> Result<()> {
    if emoji.is_empty() || emoji.contains('/') || emoji.contains('?') || emoji.contains('#') {
        bail!("Invalid emoji: must not contain path separators, got {emoji:?}");
    }
    Ok(())
}

/// Discord API error codes that will never succeed on retry — the caller must
/// intervene (fix token, grant permissions, etc.).
const FATAL_DISCORD_ERROR_CODES: &[u64] = &[
    10003, // Unknown Channel
    10004, // Unknown Guild
    10008, // Unknown Message
    10015, // Unknown Thread
    40001, // Unauthorized
    50001, // Missing Access
    50013, // Missing Permissions
    50035, // Invalid Form Body
];

/// Parsed Discord error response body.
#[derive(Deserialize)]
struct DiscordErrorResponse {
    code: Option<u64>,
    message: Option<String>,
}

/// Returns `true` if the given HTTP status + error code combination is non-recoverable.
pub(crate) fn is_fatal_discord_error(status: u16, code: Option<u64>) -> bool {
    matches!(status, 401 | 403 | 404)
        || code.is_some_and(|c| FATAL_DISCORD_ERROR_CODES.contains(&c))
}

/// Format a human-readable hint for a Discord API error code.
fn discord_error_hint(code: u64) -> &'static str {
    match code {
        10003 => " — channel does not exist or Borg cannot see it",
        10004 => " — guild does not exist or Borg is not a member",
        10008 => " — message was deleted",
        10015 => " — thread does not exist",
        40001 => " — check DISCORD_BOT_TOKEN",
        40005 => " — file exceeds Discord's size limit",
        50001 => " — Borg lacks access to this channel",
        50013 => " — Borg is missing required permissions",
        50035 => " — request body is malformed",
        _ => "",
    }
}

/// A client for the Discord REST API.
#[derive(Clone)]
pub struct DiscordClient {
    client: Client,
    token: String,
    application_id: Option<String>,
}

impl DiscordClient {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Discord HTTP client")?,
            token: token.to_string(),
            application_id: None,
        })
    }

    /// Set the application ID (typically the bot user ID from get_current_user).
    pub fn set_application_id(&mut self, id: String) {
        self.application_id = Some(id);
    }

    /// Get the current bot user via GET /users/@me.
    pub async fn get_current_user(&self) -> Result<CurrentUser> {
        let resp = self
            .client
            .get(format!("{DISCORD_API_BASE}/users/@me"))
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await
            .context("Failed to call GET /users/@me")?;

        let status = resp.status();
        if !status.is_success() {
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("Failed to read Discord error response body: {e}");
                    String::new()
                }
            };
            bail!("GET /users/@me failed ({status}): {body}");
        }

        resp.json()
            .await
            .context("Failed to parse /users/@me response")
    }

    /// Create an interaction response (initial reply to an interaction).
    ///
    /// This endpoint does NOT require a bot authorization header — it uses
    /// the interaction token in the URL path.
    pub async fn create_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        response: &InteractionResponse,
    ) -> Result<()> {
        let url = format!(
            "{DISCORD_API_BASE}/interactions/{interaction_id}/{interaction_token}/callback"
        );
        let client = self.client.clone();
        let response = response.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let response = response.clone();
            async move {
                client
                    .post(&url)
                    .json(&response)
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Edit the original interaction response (for deferred replies).
    ///
    /// Automatically chunks text at 2000 characters. For messages exceeding
    /// the limit, the first chunk is sent as an edit and subsequent chunks
    /// are sent as follow-up messages.
    pub async fn edit_original_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        text: &str,
    ) -> Result<()> {
        let chunks = chunker::chunk_text_nonempty(text, DISCORD_MESSAGE_CHUNK_SIZE);

        // Edit the original response with the first chunk
        let url = format!(
            "{DISCORD_API_BASE}/webhooks/{application_id}/{interaction_token}/messages/@original"
        );
        let chunk0 = chunks[0].clone();
        let client = self.client.clone();
        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let chunk0 = chunk0.clone();
            async move {
                client
                    .patch(&url)
                    .json(&json!({ "content": chunk0 }))
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        // Send remaining chunks as follow-up messages
        for chunk in &chunks[1..] {
            let followup_url =
                format!("{DISCORD_API_BASE}/webhooks/{application_id}/{interaction_token}");
            let chunk = chunk.clone();
            let client = self.client.clone();
            self.send_with_retry(move || {
                let client = client.clone();
                let url = followup_url.clone();
                let chunk = chunk.clone();
                async move {
                    client
                        .post(&url)
                        .json(&json!({ "content": chunk }))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;
        }

        Ok(())
    }

    /// Send a message to a channel, chunking at 2000 characters.
    pub async fn send_message(&self, channel_id: &str, text: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        let chunks = chunker::chunk_text_nonempty(text, DISCORD_MESSAGE_CHUNK_SIZE);

        for chunk in &chunks {
            let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
            let body = CreateMessageRequest {
                content: chunk.clone(),
            };
            let token = self.token.clone();
            let client = self.client.clone();
            self.send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let body = body.clone();
                let token = token.clone();
                async move {
                    client
                        .post(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .json(&body)
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;
        }

        Ok(())
    }

    /// Trigger a typing indicator in a channel.
    /// POST /channels/{channel_id}/typing
    pub async fn trigger_typing_indicator(&self, channel_id: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/typing");
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .post(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .header("Content-Length", "0")
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Add a reaction to a message.
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        validate_emoji(emoji)?;
        let url = format!(
            "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me"
        );
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .put(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .header("Content-Length", "0")
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Register global application commands with Discord.
    /// Uses `PUT /applications/{app_id}/commands` to bulk overwrite all commands.
    pub async fn register_global_commands(
        &self,
        application_id: &str,
        commands: &[DiscordCommandPayload],
    ) -> Result<()> {
        let url = format!("{DISCORD_API_BASE}/applications/{application_id}/commands");
        let token = self.token.clone();
        let body = serde_json::to_value(commands).context("Failed to serialize commands")?;
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            let body = body.clone();
            async move {
                client
                    .put(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .json(&body)
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Fetch recent messages from a Discord channel via GET /channels/{id}/messages.
    pub async fn get_channel_messages(
        &self,
        channel_id: &str,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>> {
        self.get_channel_messages_query(
            channel_id,
            &MessageQuery {
                limit,
                ..Default::default()
            },
        )
        .await
    }

    /// Fetch messages from a Discord channel with pagination parameters.
    pub async fn get_channel_messages_query(
        &self,
        channel_id: &str,
        query: &MessageQuery,
    ) -> Result<Vec<ChannelMessage>> {
        validate_snowflake(channel_id, "channel_id")?;
        let limit = query.limit.clamp(1, 100);
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let token = self.token.clone();
        let client = self.client.clone();

        let mut params: Vec<(String, String)> = vec![("limit".into(), limit.to_string())];
        if let Some(ref before) = query.before {
            params.push(("before".into(), before.clone()));
        }
        if let Some(ref after) = query.after {
            params.push(("after".into(), after.clone()));
        }
        if let Some(ref around) = query.around {
            params.push(("around".into(), around.clone()));
        }

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                let params = params.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .query(&params)
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        let raw_messages: Vec<RawChannelMessage> = resp
            .json()
            .await
            .context("Failed to parse channel messages response")?;

        Ok(raw_messages.into_iter().map(ChannelMessage::from).collect())
    }

    /// Edit a message in a channel.
    pub async fn edit_message(&self, channel_id: &str, message_id: &str, text: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}");
        let token = self.token.clone();
        let client = self.client.clone();
        let body = json!({ "content": text });

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            let body = body.clone();
            async move {
                client
                    .patch(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .json(&body)
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Delete a message from a channel.
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .delete(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Fetch a single message by ID.
    pub async fn fetch_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<ChannelMessage> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        let raw: RawChannelMessage = resp
            .json()
            .await
            .context("Failed to parse message response")?;

        Ok(ChannelMessage::from(raw))
    }

    /// Remove the bot's own reaction from a message.
    pub async fn remove_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        validate_emoji(emoji)?;
        let url = format!(
            "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me"
        );
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .delete(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Fetch users who reacted with a specific emoji.
    pub async fn fetch_reactions(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<Vec<ReactionUser>> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        validate_emoji(emoji)?;
        let url = format!(
            "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{emoji}"
        );
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        resp.json()
            .await
            .context("Failed to parse reactions response")
    }

    /// Pin a message in a channel.
    pub async fn pin_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/pins/{message_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .put(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .header("Content-Length", "0")
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Unpin a message from a channel.
    pub async fn unpin_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/pins/{message_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            async move {
                client
                    .delete(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// List pinned messages in a channel.
    pub async fn list_pins(&self, channel_id: &str) -> Result<Vec<ChannelMessage>> {
        validate_snowflake(channel_id, "channel_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/pins");
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        let raw_messages: Vec<RawChannelMessage> =
            resp.json().await.context("Failed to parse pins response")?;

        Ok(raw_messages.into_iter().map(ChannelMessage::from).collect())
    }

    /// Create a thread from a message.
    pub async fn create_thread(
        &self,
        channel_id: &str,
        message_id: &str,
        name: &str,
    ) -> Result<ThreadInfo> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_snowflake(message_id, "message_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/threads");
        let token = self.token.clone();
        let client = self.client.clone();
        let body = json!({ "name": name, "auto_archive_duration": 1440 });

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                let body = body.clone();
                async move {
                    client
                        .post(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .json(&body)
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        resp.json()
            .await
            .context("Failed to parse create thread response")
    }

    /// List active threads in a guild.
    pub async fn list_active_threads(&self, guild_id: &str) -> Result<Vec<ThreadInfo>> {
        validate_snowflake(guild_id, "guild_id")?;
        let url = format!("{DISCORD_API_BASE}/guilds/{guild_id}/threads/active");
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse active threads response")?;

        let threads: Vec<ThreadInfo> = body
            .get("threads")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(threads)
    }

    /// Get info about a guild member.
    pub async fn get_member_info(&self, guild_id: &str, user_id: &str) -> Result<MemberInfo> {
        validate_snowflake(guild_id, "guild_id")?;
        validate_snowflake(user_id, "user_id")?;
        let url = format!("{DISCORD_API_BASE}/guilds/{guild_id}/members/{user_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        resp.json()
            .await
            .context("Failed to parse member info response")
    }

    /// Get info about a channel.
    pub async fn get_channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
        validate_snowflake(channel_id, "channel_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}");
        let token = self.token.clone();
        let client = self.client.clone();

        let resp = self
            .send_with_retry(move || {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                async move {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bot {token}"))
                        .send()
                        .await
                        .context("Discord API request failed")
                }
            })
            .await?;

        resp.json()
            .await
            .context("Failed to parse channel info response")
    }

    /// Upload a file to a channel as a message attachment.
    pub async fn upload_file(
        &self,
        channel_id: &str,
        content: &[u8],
        filename: &str,
    ) -> Result<()> {
        validate_snowflake(channel_id, "channel_id")?;
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let token = self.token.clone();
        let client = self.client.clone();
        let content = content.to_vec();
        let filename = filename.to_string();

        self.send_with_retry(move || {
            let client = client.clone();
            let url = url.clone();
            let token = token.clone();
            let content = content.clone();
            let filename = filename.clone();
            async move {
                let form = reqwest::multipart::Form::new().part(
                    "files[0]",
                    reqwest::multipart::Part::bytes(content).file_name(filename),
                );
                client
                    .post(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .multipart(form)
                    .send()
                    .await
                    .context("Discord API request failed")
            }
        })
        .await?;

        Ok(())
    }

    /// Send a request with 429 rate-limit retry logic using the shared retry helper.
    async fn send_with_retry<F, Fut>(&self, make_request: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response>>,
    {
        let policy = RateLimitPolicy {
            service_name: "Discord",
            ..RateLimitPolicy::default()
        };

        let resp = send_with_rate_limit_retry(&policy, make_request).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let parsed: Option<DiscordErrorResponse> = serde_json::from_str(&body).ok();
            let code = parsed.as_ref().and_then(|p| p.code);
            let msg = parsed
                .as_ref()
                .and_then(|p| p.message.as_deref())
                .unwrap_or(&body);
            let hint = code.map(discord_error_hint).unwrap_or("");

            if is_fatal_discord_error(status.as_u16(), code) {
                error!(
                    "Discord API FATAL error ({status}): {msg}{hint} — this will not be retried"
                );
                bail!("Discord API FATAL ({status}): {msg}{hint}");
            }

            bail!("Discord API error ({status}): {msg}{hint}");
        }

        Ok(resp)
    }
}

/// Payload for registering a Discord global application command.
#[derive(Serialize, Clone)]
pub struct DiscordCommandPayload {
    pub name: String,
    pub description: String,
    /// Command type: 1 = CHAT_INPUT (slash command)
    #[serde(rename = "type")]
    pub command_type: u8,
}

#[async_trait]
impl NativeCommandRegistration for DiscordClient {
    async fn register_commands(&self, commands: &[CommandDef]) -> Result<()> {
        let app_id = self
            .application_id
            .as_deref()
            .context("Discord application_id not set")?;

        let payloads: Vec<DiscordCommandPayload> = commands
            .iter()
            .map(|c| DiscordCommandPayload {
                name: c.name.to_string(),
                description: c.description.to_string(),
                command_type: 1,
            })
            .collect();

        self.register_global_commands(app_id, &payloads).await
    }
}

impl From<RawChannelMessage> for ChannelMessage {
    fn from(m: RawChannelMessage) -> Self {
        Self {
            author_id: m.author.id,
            author_username: m.author.username,
            content: m.content,
            id: m.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/users/@me"),
            "https://discord.com/api/v10/users/@me"
        );
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages"),
            "https://discord.com/api/v10/channels/123/messages"
        );
        assert_eq!(
            format!("{DISCORD_API_BASE}/interactions/int1/tok/callback"),
            "https://discord.com/api/v10/interactions/int1/tok/callback"
        );
        assert_eq!(
            format!("{DISCORD_API_BASE}/webhooks/app1/tok/messages/@original"),
            "https://discord.com/api/v10/webhooks/app1/tok/messages/@original"
        );
        assert_eq!(
            format!("{DISCORD_API_BASE}/webhooks/app1/tok"),
            "https://discord.com/api/v10/webhooks/app1/tok"
        );
    }

    #[test]
    fn create_message_request_serialization() {
        let req = CreateMessageRequest {
            content: "hello discord".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["content"], "hello discord");
    }

    #[test]
    fn interaction_response_serialization() {
        let resp = InteractionResponse::deferred();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], 5);
    }

    #[test]
    fn chunking_at_2000() {
        let long_text: String = "a".repeat(4500);
        let chunks = chunker::chunk_text(&long_text, DISCORD_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[1].len(), 2000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn short_text_single_chunk() {
        let text = "short message";
        let chunks = chunker::chunk_text(text, DISCORD_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks, vec!["short message"]);
    }

    #[test]
    fn client_construction() {
        let client = DiscordClient::new("test-token").unwrap();
        assert_eq!(client.token, "test-token");
    }

    #[test]
    fn reaction_url_construction() {
        let channel_id = "123";
        let message_id = "456";
        let emoji = "%F0%9F%91%8D"; // URL-encoded thumbs up
        let url = format!(
            "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me"
        );
        assert_eq!(
            url,
            "https://discord.com/api/v10/channels/123/messages/456/reactions/%F0%9F%91%8D/@me"
        );
    }

    // ── Error classification tests ──

    #[test]
    fn is_fatal_discord_error_classifies_auth() {
        assert!(is_fatal_discord_error(401, None));
        assert!(is_fatal_discord_error(200, Some(40001)));
    }

    #[test]
    fn is_fatal_discord_error_classifies_permissions() {
        assert!(is_fatal_discord_error(403, None));
        assert!(is_fatal_discord_error(404, None));
        assert!(is_fatal_discord_error(200, Some(50001)));
        assert!(is_fatal_discord_error(200, Some(50013)));
    }

    #[test]
    fn is_fatal_discord_error_classifies_channel_errors() {
        assert!(is_fatal_discord_error(200, Some(10003)));
        assert!(is_fatal_discord_error(200, Some(10004)));
        assert!(is_fatal_discord_error(200, Some(10008)));
    }

    #[test]
    fn is_fatal_discord_error_excludes_transient() {
        assert!(!is_fatal_discord_error(429, None));
        assert!(!is_fatal_discord_error(500, None));
        assert!(!is_fatal_discord_error(502, None));
        assert!(!is_fatal_discord_error(503, None));
        assert!(!is_fatal_discord_error(200, None));
        assert!(!is_fatal_discord_error(200, Some(99999)));
    }

    #[test]
    fn discord_error_hint_present_for_known() {
        assert!(discord_error_hint(10003).contains("channel"));
        assert!(discord_error_hint(10004).contains("guild"));
        assert!(discord_error_hint(40001).contains("DISCORD_BOT_TOKEN"));
        assert!(discord_error_hint(50001).contains("access"));
        assert!(discord_error_hint(50013).contains("permissions"));
        assert!(discord_error_hint(50035).contains("malformed"));
        assert!(discord_error_hint(40005).contains("size"));
    }

    #[test]
    fn discord_error_hint_empty_for_unknown() {
        assert_eq!(discord_error_hint(99999), "");
        assert_eq!(discord_error_hint(0), "");
    }

    #[test]
    fn discord_error_response_deserialization() {
        let json = r#"{"code": 50001, "message": "Missing Access"}"#;
        let parsed: DiscordErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.code, Some(50001));
        assert_eq!(parsed.message.as_deref(), Some("Missing Access"));
    }

    #[test]
    fn discord_error_response_partial() {
        let json = r#"{"message": "Unknown error"}"#;
        let parsed: DiscordErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.code, None);
        assert_eq!(parsed.message.as_deref(), Some("Unknown error"));
    }

    // ── Channel history tests ──

    #[test]
    fn channel_messages_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/789/messages"),
            "https://discord.com/api/v10/channels/789/messages"
        );
    }

    #[test]
    fn validate_snowflake_accepts_numeric() {
        assert!(validate_snowflake("123456789", "test").is_ok());
        assert!(validate_snowflake("0", "test").is_ok());
    }

    #[test]
    fn validate_snowflake_rejects_non_numeric() {
        assert!(validate_snowflake("", "test").is_err());
        assert!(validate_snowflake("abc", "test").is_err());
        assert!(validate_snowflake("123/../../users/@me", "test").is_err());
        assert!(validate_snowflake("123 456", "test").is_err());
    }

    #[test]
    fn validate_emoji_accepts_valid() {
        assert!(validate_emoji("%E2%9C%85").is_ok());
        assert!(validate_emoji("%F0%9F%91%8D").is_ok());
        assert!(validate_emoji("custom_emoji:123456").is_ok());
    }

    #[test]
    fn validate_emoji_rejects_path_traversal() {
        assert!(validate_emoji("").is_err());
        assert!(validate_emoji("../../../users/@me").is_err());
        assert!(validate_emoji("emoji/test").is_err());
        assert!(validate_emoji("emoji?foo=bar").is_err());
        assert!(validate_emoji("emoji#frag").is_err());
    }

    #[test]
    fn channel_message_deserialization() {
        let json = r#"[
            {
                "id": "msg1",
                "content": "hello world",
                "author": { "id": "u1", "username": "alice" }
            },
            {
                "id": "msg2",
                "content": "hi there",
                "author": { "id": "u2", "username": "bob" }
            }
        ]"#;
        let raw: Vec<RawChannelMessage> = serde_json::from_str(json).unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].id, "msg1");
        assert_eq!(raw[0].author.username, "alice");
        assert_eq!(raw[1].content, "hi there");
    }

    #[test]
    fn channel_message_from_raw() {
        let raw = RawChannelMessage {
            id: "m1".into(),
            content: "hello".into(),
            author: super::super::types::RawAuthor {
                id: "u1".into(),
                username: "alice".into(),
            },
        };
        let msg = ChannelMessage::from(raw);
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.author_id, "u1");
        assert_eq!(msg.author_username, "alice");
        assert_eq!(msg.content, "hello");
    }

    // ── New API method URL construction tests ──

    #[test]
    fn edit_message_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages/456"),
            "https://discord.com/api/v10/channels/123/messages/456"
        );
    }

    #[test]
    fn delete_message_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages/456"),
            "https://discord.com/api/v10/channels/123/messages/456"
        );
    }

    #[test]
    fn fetch_message_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/111/messages/222"),
            "https://discord.com/api/v10/channels/111/messages/222"
        );
    }

    #[test]
    fn remove_reaction_url_construction() {
        let emoji = "%E2%9C%85";
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages/456/reactions/{emoji}/@me"),
            "https://discord.com/api/v10/channels/123/messages/456/reactions/%E2%9C%85/@me"
        );
    }

    #[test]
    fn fetch_reactions_url_construction() {
        let emoji = "%F0%9F%91%8D";
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages/456/reactions/{emoji}"),
            "https://discord.com/api/v10/channels/123/messages/456/reactions/%F0%9F%91%8D"
        );
    }

    #[test]
    fn pin_message_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/pins/456"),
            "https://discord.com/api/v10/channels/123/pins/456"
        );
    }

    #[test]
    fn list_pins_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/pins"),
            "https://discord.com/api/v10/channels/123/pins"
        );
    }

    #[test]
    fn create_thread_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages/456/threads"),
            "https://discord.com/api/v10/channels/123/messages/456/threads"
        );
    }

    #[test]
    fn list_active_threads_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/guilds/789/threads/active"),
            "https://discord.com/api/v10/guilds/789/threads/active"
        );
    }

    #[test]
    fn get_member_info_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/guilds/789/members/123"),
            "https://discord.com/api/v10/guilds/789/members/123"
        );
    }

    #[test]
    fn get_channel_info_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123"),
            "https://discord.com/api/v10/channels/123"
        );
    }

    #[test]
    fn upload_file_url_construction() {
        assert_eq!(
            format!("{DISCORD_API_BASE}/channels/123/messages"),
            "https://discord.com/api/v10/channels/123/messages"
        );
    }

    #[test]
    fn create_thread_body_serialization() {
        let body = serde_json::json!({ "name": "my-thread", "auto_archive_duration": 1440 });
        assert_eq!(body["name"], "my-thread");
        assert_eq!(body["auto_archive_duration"], 1440);
    }

    #[test]
    fn edit_message_body_serialization() {
        let body = serde_json::json!({ "content": "updated text" });
        assert_eq!(body["content"], "updated text");
    }

    #[test]
    fn message_query_params_construction() {
        let query = MessageQuery {
            limit: 50,
            before: Some("999".into()),
            after: None,
            around: None,
        };
        let mut params: Vec<(String, String)> = vec![("limit".into(), query.limit.to_string())];
        if let Some(ref before) = query.before {
            params.push(("before".into(), before.clone()));
        }
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("limit".into(), "50".into()));
        assert_eq!(params[1], ("before".into(), "999".into()));
    }

    #[test]
    fn is_fatal_discord_error_classifies_thread_errors() {
        assert!(is_fatal_discord_error(200, Some(10015)));
    }

    #[test]
    fn discord_error_hint_thread() {
        assert!(discord_error_hint(10015).contains("thread"));
    }

    #[test]
    fn active_threads_response_deserialization() {
        let json = r#"{"threads": [{"id": "t1", "name": "thread-1"}, {"id": "t2"}]}"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let threads: Vec<super::super::types::ThreadInfo> = body
            .get("threads")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].id, "t1");
        assert_eq!(threads[0].name.as_deref(), Some("thread-1"));
        assert!(threads[1].name.is_none());
    }
}
