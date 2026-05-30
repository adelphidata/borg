//! Native channel send helpers used by heartbeat + approval greeting.

use anyhow::{anyhow, Result};
use cortex::config::Config;

/// Send `text` to `sender_id` on `channel`, optionally targeted at a thread.
pub async fn send(config: &Config, channel: &str, sender_id: &str, text: &str) -> Result<()> {
    send_threaded(config, channel, sender_id, text, None).await
}

async fn send_threaded(
    config: &Config,
    channel: &str,
    sender_id: &str,
    text: &str,
    thread_id: Option<&str>,
) -> Result<()> {
    match channel {
        "telegram" => {
            let token = config
                .resolve_credential_or_env("TELEGRAM_BOT_TOKEN")
                .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN not configured"))?;
            let client = gateway::telegram::api::TelegramClient::new(&token)?;
            let chat_id: i64 = sender_id
                .parse()
                .map_err(|_| anyhow!("Invalid Telegram chat_id: {sender_id}"))?;
            let message_thread_id: Option<i64> = thread_id.and_then(|s| s.parse().ok());
            client
                .send_message(chat_id, text, None, None, message_thread_id)
                .await
        }
        "slack" => {
            let token = config
                .resolve_credential_or_env("SLACK_BOT_TOKEN")
                .ok_or_else(|| anyhow!("SLACK_BOT_TOKEN not configured"))?;
            let client = gateway::slack::api::SlackClient::new(&token)?;
            client
                .post_message(sender_id, text, thread_id)
                .await
                .map(|_| ())
        }
        "discord" => {
            let token = config
                .resolve_credential_or_env("DISCORD_BOT_TOKEN")
                .ok_or_else(|| anyhow!("DISCORD_BOT_TOKEN not configured"))?;
            let client = gateway::discord::api::DiscordClient::new(&token)?;
            let channel_id = thread_id.unwrap_or(sender_id);
            client.send_message(channel_id, text).await
        }
        other => Err(anyhow!("Channel '{other}' not supported for delivery")),
    }
}
