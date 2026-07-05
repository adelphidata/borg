use std::sync::Arc;
use std::time::Duration;

use super::api::TelegramClient;
use crate::channel::channel_trait::TypingHandle;
use crate::typing_keepalive::TypingKeepaliveConfig;

/// Keepalive interval for re-sending typing action (Telegram typing expires ~5s).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);

/// Start a typing indicator keepalive for `chat_id`.
///
/// Spawns a background task that calls Telegram's sendChatAction("typing")
/// every 4 seconds to keep the indicator visible (it expires after ~5s).
/// Auto-stops after TTL. Used by both the webhook channel's `start_typing`
/// and the poll-mode message path in `server.rs`.
pub fn start_typing_keepalive(client: Arc<TelegramClient>, chat_id: i64) -> TypingHandle {
    TypingHandle::spawn_keepalive(
        TypingKeepaliveConfig {
            keepalive_interval: KEEPALIVE_INTERVAL,
            label: "telegram",
        },
        move || {
            let client = client.clone();
            async move { client.send_typing(chat_id).await }
        },
    )
}
