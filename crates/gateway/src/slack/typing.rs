use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use super::api::SlackClient;
use crate::typing_keepalive::{self, TypingIndicatorHandle, TypingKeepaliveConfig};

/// Keepalive interval for re-sending typing status (matches OpenClaw).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(3);

/// Reaction emoji added to the user's message while typing.
const TYPING_REACTION: &str = "thinking_face";

/// Handle to a running typing indicator. Call `stop()` to clean up.
pub struct TypingIndicator {
    inner: TypingIndicatorHandle,
    channel: String,
    thread_ts: Option<String>,
    message_ts: Option<String>,
    client: Arc<SlackClient>,
}

impl TypingIndicator {
    /// Start typing indicator with keepalive loop.
    ///
    /// Spawns a background task that:
    /// 1. Sets thread status to "is typing..." via `assistant.threads.setStatus`
    /// 2. Adds `:thinking_face:` reaction to the user's message
    /// 3. Every 3s, re-sends setStatus to keep it visible (keepalive)
    /// 4. After TTL, auto-stops with warning log
    pub fn start(
        client: Arc<SlackClient>,
        channel: String,
        thread_ts: Option<String>,
        message_ts: Option<String>,
    ) -> Self {
        let bg_client = client.clone();
        let bg_channel = channel.clone();
        let bg_thread_ts = thread_ts.clone();
        let bg_message_ts = message_ts.clone();

        let inner = TypingIndicatorHandle::start(move |stop_rx| {
            Box::pin(async move {
                // Add reaction to user's message before starting keepalive
                if let Some(ref ts) = bg_message_ts {
                    bg_client
                        .add_reaction(&bg_channel, ts, TYPING_REACTION)
                        .await;
                }

                let config = TypingKeepaliveConfig {
                    keepalive_interval: KEEPALIVE_INTERVAL,
                    label: "slack",
                };
                typing_keepalive::run_keepalive(config, stop_rx, || {
                    let client = bg_client.clone();
                    let channel = bg_channel.clone();
                    let thread_ts = bg_thread_ts.clone();
                    async move {
                        client
                            .set_thread_status(&channel, thread_ts.as_deref(), "is typing...")
                            .await
                    }
                })
                .await;
            })
        });

        Self {
            inner,
            channel,
            thread_ts,
            message_ts,
            client,
        }
    }

    /// Stop the keepalive loop, clear status, remove reaction.
    pub async fn stop(mut self) {
        // Signal and wait for background task
        self.inner.stop().await;

        // Clear thread status (empty string = clear, matching OpenClaw)
        if let Err(e) = self
            .client
            .set_thread_status(&self.channel, self.thread_ts.as_deref(), "")
            .await
        {
            warn!("[typing] Failed to clear thread status: {e}");
        }

        // Remove reaction
        if let Some(ref ts) = self.message_ts {
            self.client
                .remove_reaction(&self.channel, ts, TYPING_REACTION)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_openclaw() {
        assert_eq!(KEEPALIVE_INTERVAL, Duration::from_secs(3));
        assert_eq!(TYPING_REACTION, "thinking_face");
    }
}
