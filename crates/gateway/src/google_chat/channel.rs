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
    /// Config handle — needed to build the STT pipeline so voice
    /// attachments get transcribed before the agent runs.
    pub config: cortex::config::Config,
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
        let mut inbound = match super::handle_google_chat_webhook(body, self.token.as_deref())? {
            Some(msg) => msg,
            None => return Ok(WebhookOutcome::Skip),
        };

        // Inbound voice transcription. The parse step embeds the
        // attachment's `resourceName` in `data`; download via the
        // `media.download` endpoint (Bearer auth) and run STT.
        if let Some(transcriber) =
            cortex::media::understanding::AudioTranscriber::from_config(&self.config)
        {
            let mut transcripts: Vec<String> = Vec::new();
            for att in &inbound.attachments {
                if !att.mime_type.starts_with("audio/") {
                    continue;
                }
                match self.client.download_attachment(&att.data).await {
                    Ok(bytes) => {
                        let fname = att
                            .filename
                            .clone()
                            .unwrap_or_else(|| "gchat-voice.audio".to_string());
                        match transcriber
                            .transcribe(&bytes, &att.mime_type, &fname, None)
                            .await
                        {
                            Ok((transcript, _)) => transcripts.push(transcript),
                            Err(e) => {
                                tracing::warn!(error = %e, "google_chat: audio transcription failed")
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "google_chat: attachment download failed"),
                }
            }
            if !transcripts.is_empty() {
                let joined = transcripts.join(" ");
                inbound.text = if inbound.text.trim().is_empty() {
                    format!("[Voice transcript]: {joined}")
                } else {
                    format!("{}\n[Voice transcript]: {joined}", inbound.text)
                };
            }
        }

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
