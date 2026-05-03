//! `NativeChannel` implementation for Twilio (SMS + WhatsApp).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use cortex::config::Config;

use crate::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;

use super::api::TwilioClient;
use super::types::TwilioChannelType;

/// Twilio-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct TwilioResponseContext {
    channel_type: String,
    sender: String,
    from_number: Option<String>,
}

/// Twilio native channel implementation (handles SMS, WhatsApp).
pub struct TwilioChannel {
    /// Shared Twilio REST API client.
    pub client: Arc<TwilioClient>,
    /// Auth token for webhook signature verification.
    pub auth_token: Option<String>,
    /// Outbound SMS phone number (E.164 format).
    pub phone_number: Option<String>,
    /// Outbound WhatsApp phone number (E.164 format).
    pub whatsapp_number: Option<String>,
    /// Application configuration.
    pub config: Config,
}

#[async_trait]
impl NativeChannel for TwilioChannel {
    fn names(&self) -> Vec<&str> {
        vec!["twilio", "whatsapp", "sms"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        // Verify signature if auth token is available
        if let Some(ref auth_token) = self.auth_token {
            if let Some(ref public_url) = self.config.gateway.public_url {
                let webhook_url = format!("{public_url}/webhook/twilio");
                crate::twilio::verify::verify_twilio_signature(
                    headers,
                    &webhook_url,
                    body,
                    auth_token,
                )?;
            } else {
                anyhow::bail!(
                    "Twilio signature verification unavailable: gateway.public_url not configured"
                );
            }
        }

        let parsed = super::parse::parse_webhook(body)?;

        let channel_type = parsed.channel_type;
        let sender = parsed.message.sender_id.clone();
        let session_key = format!("{}:{}", channel_type.as_str(), sender);

        let from_number = match channel_type {
            TwilioChannelType::WhatsApp => self.whatsapp_number.clone(),
            TwilioChannelType::Sms => self.phone_number.clone(),
        };

        let response_context = serde_json::to_value(TwilioResponseContext {
            channel_type: channel_type.as_str().to_string(),
            sender,
            from_number,
        })?;

        Ok(WebhookOutcome::Message {
            inbound: parsed.message,
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
        let ctx: TwilioResponseContext = serde_json::from_value(response_context.clone())?;

        // SMS / WhatsApp have no threading; mark `/btw` answers visually so
        // the user can recognise side replies among the main conversation.
        let response_text = crate::btw_dispatch::maybe_prefix_btw(response_text, response_context);
        let response_text = response_text.as_str();

        let from = match ctx.from_number {
            Some(ref n) => n.as_str(),
            None => {
                tracing::warn!(
                    "No outbound phone number configured for {}",
                    ctx.channel_type
                );
                return Ok(());
            }
        };

        let send_result = if ctx.channel_type == "whatsapp" {
            self.client
                .send_whatsapp(from, &ctx.sender, response_text)
                .await
        } else {
            self.client.send_sms(from, &ctx.sender, response_text).await
        };

        if let Err(e) = send_result {
            health
                .write()
                .await
                .record_error(&ctx.channel_type, &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound(&ctx.channel_type);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_context_roundtrip() {
        let ctx = TwilioResponseContext {
            channel_type: "whatsapp".to_string(),
            sender: "+1234567890".to_string(),
            from_number: Some("+0987654321".to_string()),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: TwilioResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_type, "whatsapp");
        assert_eq!(back.sender, "+1234567890");
    }
}
