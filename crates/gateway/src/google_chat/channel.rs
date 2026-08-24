//! `NativeChannel` implementation for Google Chat.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

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
            let mut failed_kinds: Vec<&'static str> = Vec::new();
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
                        transcripts.extend(
                            voice::transcribe_or_warn(
                                &transcriber,
                                &bytes,
                                &att.mime_type,
                                &fname,
                                "google_chat",
                            )
                            .await,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "google_chat: attachment download failed");
                        failed_kinds.push(crate::message_processing::media_failure::kind_for_mime(
                            &att.mime_type,
                        ));
                    }
                }
            }
            for kind in failed_kinds {
                crate::message_processing::media_failure::append_download_failure(
                    &mut inbound.text,
                    kind,
                );
            }
            voice::append_voice_transcripts(&mut inbound.text, &transcripts);
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
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn stub_channel(server: &MockServer, config: cortex::config::Config) -> GoogleChatChannel {
        GoogleChatChannel {
            client: Arc::new(GoogleChatClient::with_base("tok-1", &server.uri()).expect("client")),
            token: Some("hook-token".to_string()),
            config,
        }
    }

    fn health() -> Arc<RwLock<ChannelHealthRegistry>> {
        let mut reg = ChannelHealthRegistry::new();
        reg.register("google-chat");
        Arc::new(RwLock::new(reg))
    }

    #[test]
    fn names_cover_every_accepted_alias() {
        // The dispatcher registry keys on these strings; losing an alias
        // silently 404s webhooks configured under it.
        let names = |c: &GoogleChatChannel| -> Vec<String> {
            c.names().iter().map(|s| s.to_string()).collect()
        };
        let channel = GoogleChatChannel {
            client: Arc::new(GoogleChatClient::new("t").expect("client")),
            token: None,
            config: cortex::config::Config::default(),
        };
        assert_eq!(
            names(&channel),
            vec!["google-chat", "google_chat", "googlechat"]
        );
    }

    #[tokio::test]
    async fn send_response_delivers_to_space_and_records_outbound() {
        // send_response must actually reach the API (thread inherited from
        // the stored context) and record the outbound in health — a
        // stubbed-out implementation would do neither.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/spaces/S9/messages"))
            .and(body_partial_json(serde_json::json!({
                "text": "hello there",
                "thread": {"name": "spaces/S9/threads/T2"},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let channel = stub_channel(&server, cortex::config::Config::default());
        let health = health();
        let ctx = serde_json::json!({
            "space_name": "spaces/S9",
            "thread_name": "spaces/S9/threads/T2",
        });

        channel
            .send_response("hello there", &ctx, &health)
            .await
            .expect("send_response must succeed on 200");

        let snap = health.read().await.snapshot();
        let row = snap
            .iter()
            .find(|s| s.name == "google-chat")
            .expect("registered");
        assert_eq!(row.outbound_count, 1);
        assert_eq!(row.error_count, 0);
    }

    #[tokio::test]
    async fn send_response_api_failure_errs_and_records_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let channel = stub_channel(&server, cortex::config::Config::default());
        let health = health();
        let ctx = serde_json::json!({"space_name": "spaces/S9", "thread_name": null});

        channel
            .send_response("hi", &ctx, &health)
            .await
            .expect_err("500 must propagate as an error");

        let snap = health.read().await.snapshot();
        let row = snap
            .iter()
            .find(|s| s.name == "google-chat")
            .expect("registered");
        assert_eq!(row.error_count, 1);
        assert_eq!(row.outbound_count, 0);
    }

    fn stt_config() -> cortex::config::Config {
        // A transcriber must exist for the attachment loop to run at all.
        // Key comes from a test-unique env var so no real credentials leak in.
        unsafe { std::env::set_var("BORG_TEST_GCHAT_STT_KEY", "test-key") };
        let mut config = cortex::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "openai".to_string(),
            model: None,
            api_key_env: Some("BORG_TEST_GCHAT_STT_KEY".to_string()),
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        }];
        config
    }

    #[tokio::test]
    async fn handle_webhook_downloads_only_audio_attachments() {
        // A message carries one audio and one image attachment. Only the
        // audio one goes through the media.download + STT path; the image
        // must be skipped. The stub 500s the download so the failure
        // placeholder (not a live STT call) is the observable.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let channel = stub_channel(&server, stt_config());

        let body = serde_json::json!({
            "type": "MESSAGE",
            "token": "hook-token",
            "message": {
                "name": "spaces/S1/messages/M1",
                "sender": {"name": "users/U1", "displayName": "Alice", "type": "HUMAN"},
                "text": "listen to this",
                "attachment": [
                    {
                        "contentName": "note.ogg",
                        "contentType": "audio/ogg",
                        "attachmentDataRef": {"resourceName": "attachments/AUDIO1"}
                    },
                    {
                        "contentName": "pic.png",
                        "contentType": "image/png",
                        "attachmentDataRef": {"resourceName": "attachments/IMG1"}
                    }
                ]
            },
            "space": {"name": "spaces/S1", "displayName": "General", "type": "ROOM"}
        })
        .to_string();

        let gw_config = cortex::config::Config::default();
        let hreg = health();
        let ctx = WebhookContext {
            config: &gw_config,
            health: &hreg,
        };
        let outcome = channel
            .handle_webhook(&HeaderMap::new(), &body, &ctx)
            .await
            .expect("webhook");
        let WebhookOutcome::Message { inbound, .. } = outcome else {
            panic!("expected a message outcome");
        };

        assert!(
            inbound.text.contains("[audio could not be downloaded]"),
            "audio attachment must be fetched (and here fail): {}",
            inbound.text
        );
        assert!(
            !inbound.text.contains("[image could not be downloaded]"),
            "image attachments must not enter the voice path: {}",
            inbound.text
        );
        let reqs = server.received_requests().await.expect("recorded");
        assert_eq!(reqs.len(), 1, "exactly the audio attachment is fetched");
        assert_eq!(reqs[0].url.path(), "/media/attachments/AUDIO1");
    }

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
