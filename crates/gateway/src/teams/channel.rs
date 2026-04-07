//! `NativeChannel` implementation for Microsoft Teams.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use crate::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;

use super::api::TeamsClient;
use super::dedup::ActivityDeduplicator;

/// Teams-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct TeamsResponseContext {
    service_url: String,
    conversation_id: String,
    activity_id: String,
}

/// Microsoft Teams native channel implementation.
pub struct TeamsChannel {
    pub client: Arc<TeamsClient>,
    pub app_secret: Option<String>,
    pub dedup: Arc<Mutex<ActivityDeduplicator>>,
}

#[async_trait]
impl NativeChannel for TeamsChannel {
    fn names(&self) -> Vec<&str> {
        vec!["teams"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        let parsed = super::handle_teams_webhook(
            headers,
            body,
            self.app_secret.as_deref(),
            Some(self.dedup.as_ref()),
        )?;

        let (inbound, activity) = match parsed {
            Some(pair) => pair,
            None => {
                // Check for bot-added welcome scenario
                if let Ok(activity) = serde_json::from_str::<super::types::Activity>(body) {
                    if super::parse::is_bot_added(&activity) {
                        if let (Some(svc), Some(conv)) =
                            (&activity.service_url, &activity.conversation)
                        {
                            let welcome =
                                "Hello! I'm now available in this conversation. How can I help?";
                            let _ = self
                                .client
                                .send_to_conversation(svc, &conv.id, welcome)
                                .await;
                        }
                    }
                }
                return Ok(WebhookOutcome::Skip);
            }
        };

        let service_url = activity
            .service_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Missing service_url in Teams activity"))?;
        let conversation_id = activity
            .conversation
            .as_ref()
            .map(|c| c.id.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing conversation in Teams activity"))?;

        let session_key = inbound.session_key("teams", inbound.channel_id.as_deref().unwrap_or(""));

        let response_context = serde_json::to_value(TeamsResponseContext {
            service_url: service_url.to_string(),
            conversation_id: conversation_id.to_string(),
            activity_id: activity.id.clone(),
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
        let ctx: TeamsResponseContext = serde_json::from_value(response_context.clone())?;

        if let Err(e) = self
            .client
            .reply_to_activity(
                &ctx.service_url,
                &ctx.conversation_id,
                &ctx.activity_id,
                response_text,
            )
            .await
        {
            health.write().await.record_error("teams", &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound("teams");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_context_roundtrip() {
        let ctx = TeamsResponseContext {
            service_url: "https://smba.trafficmanager.net/amer/".to_string(),
            conversation_id: "conv1".to_string(),
            activity_id: "act1".to_string(),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TeamsResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.service_url, "https://smba.trafficmanager.net/amer/");
        assert_eq!(back.conversation_id, "conv1");
    }
}
