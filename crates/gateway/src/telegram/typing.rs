use std::sync::Arc;

use super::api::TelegramClient;
use crate::constants::TELEGRAM_TYPING_KEEPALIVE_INTERVAL;
use crate::typing_keepalive::{self, TypingIndicatorHandle, TypingKeepaliveConfig};

/// Handle to a running typing indicator. Call `stop()` to clean up.
pub struct TypingIndicator {
    inner: TypingIndicatorHandle,
}

impl TypingIndicator {
    /// Start typing indicator with keepalive loop.
    ///
    /// Spawns a background task that calls Telegram's sendChatAction("typing")
    /// every 4 seconds to keep the indicator visible (it expires after ~5s).
    /// Auto-stops after TTL.
    pub fn start(client: Arc<TelegramClient>, chat_id: i64) -> Self {
        let inner = TypingIndicatorHandle::start(move |stop_rx| {
            Box::pin(async move {
                let config = TypingKeepaliveConfig {
                    keepalive_interval: TELEGRAM_TYPING_KEEPALIVE_INTERVAL,
                    label: "telegram",
                };
                typing_keepalive::run_keepalive(config, stop_rx, || {
                    let client = client.clone();
                    async move { client.send_typing(chat_id).await }
                })
                .await;
            })
        });
        Self { inner }
    }

    /// Stop the keepalive loop and wait for the background task to finish.
    pub async fn stop(mut self) {
        self.inner.stop().await;
    }
}
