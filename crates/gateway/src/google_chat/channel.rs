//! `NativeChannel` implementation for Google Chat.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;

use super::api::GoogleChatClient;

/// Google Chat-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct GoogleChatResponseContext {
    space_name: String,
    thread_name: Option<String>,
}

/// Google Chat native channel implementation.
pub struct GoogleChatChannel {
    pub client: Arc<GoogleChatClient>,
    pub token: Option<String>,
}

#[async_trait]
impl NativeChannel for GoogleChatChannel {
    fn names(&self) -> Vec<&str> {
        vec!["google-chat", "google_chat", "googlechat"]
    }

    async fn handle_webhook(
        &self,
        _headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        let inbound = match super::handle_google_chat_webhook(body, self.token.as_deref())? {
            Some(msg) => msg,
            None => return Ok(WebhookOutcome::Skip),
        };

        let space_name = inbound.channel_id.clone().unwrap_or_default();
        let thread_name = inbound.thread_id.clone();

        let session_key = inbound.session_key("google-chat", &space_name);

        let response_context = serde_json::to_value(GoogleChatResponseContext {
            space_name,
            thread_name,
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
        let ctx: GoogleChatResponseContext = serde_json::from_value(response_context.clone())?;

        // /btw replies inherit the inbound `thread_name` so they nest in the
        // same Google Chat thread; the prefix marks them visually as side
        // answers.
        let response_text = crate::btw_dispatch::maybe_prefix_btw(response_text, response_context);

        if let Err(e) = self
            .client
            .send_message(&ctx.space_name, &response_text, ctx.thread_name.as_deref())
            .await
        {
            health
                .write()
                .await
                .record_error("google-chat", &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound("google-chat");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_context_roundtrip() {
        let ctx = GoogleChatResponseContext {
            space_name: "spaces/abc".to_string(),
            thread_name: Some("spaces/abc/threads/def".to_string()),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: GoogleChatResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.space_name, "spaces/abc");
        assert_eq!(back.thread_name.as_deref(), Some("spaces/abc/threads/def"));
    }
}
