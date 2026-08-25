//! `NativeChannel` implementation for Microsoft Teams.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

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
    /// Config handle — needed to build the inbound STT pipeline so
    /// Teams voice attachments get transcribed before the agent runs.
    pub config: cortex::config::Config,
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
        // Fail closed: refuse inbound activities when no app secret is
        // configured, rather than trusting a sender identity that is just
        // attacker-controlled JSON. Matches Slack/Discord/Telegram.
        crate::verify_common::require_verification_secret(
            "teams",
            self.app_secret.as_deref(),
            "TEAMS_APP_SECRET",
        )?;
        let parsed = super::handle_teams_webhook(
            headers,
            body,
            self.app_secret.as_deref(),
            Some(self.dedup.as_ref()),
        )?;

        let (mut inbound, activity) = match parsed {
            Some(pair) => pair,
            None => {
                if let Ok(activity) = serde_json::from_str::<super::types::Activity>(body) {
                    if super::parse::is_bot_added(&activity) {
                        if let (Some(svc), Some(conv)) =
                            (&activity.service_url, &activity.conversation)
                        {
                            let welcome =
                                "Hello! I'm now available in this conversation. How can I help?";
                            if let Err(e) = self
                                .client
                                .send_to_conversation(svc, &conv.id, welcome)
                                .await
                            {
                                tracing::warn!("Failed to send Teams welcome message: {e}");
                            }
                        }
                    }
                }
                return Ok(WebhookOutcome::Skip);
            }
        };

        // Inbound voice transcription. Teams voice attachments come as
        // `audio/*` content types on SharePoint URLs that require the
        // Bot Framework OAuth token to fetch.
        if let Some(ref atts) = activity.attachments {
            if let Some(transcriber) =
                cortex::media::understanding::AudioTranscriber::from_config(&self.config)
            {
                let mut transcripts: Vec<String> = Vec::new();
                let mut failed_kinds: Vec<&'static str> = Vec::new();
                for att in atts {
                    if !att.content_type.starts_with("audio/") {
                        continue;
                    }
                    let Some(ref url) = att.content_url else {
                        continue;
                    };
                    match self.client.download_attachment(url).await {
                        Ok(bytes) => {
                            let fname = att
                                .name
                                .clone()
                                .unwrap_or_else(|| "teams-voice.audio".to_string());
                            transcripts.extend(
                                voice::transcribe_or_warn(
                                    &transcriber,
                                    &bytes,
                                    &att.content_type,
                                    &fname,
                                    "teams",
                                )
                                .await,
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "teams: attachment download failed");
                            failed_kinds.push(
                                crate::message_processing::media_failure::kind_for_mime(
                                    &att.content_type,
                                ),
                            );
                        }
                    }
                }
                // A failed download surfaces as a placeholder line so the
                // voice-only empty-text guard below doesn't drop the turn and
                // the agent learns media existed.
                for kind in failed_kinds {
                    crate::message_processing::media_failure::append_download_failure(
                        &mut inbound.text,
                        kind,
                    );
                }
                voice::append_voice_transcripts(&mut inbound.text, &transcripts);
            }
        }

        // A voice-only message whose transcript never materialized (STT
        // unconfigured or transcription errored; download failures surface a
        // placeholder above instead of landing here) would
        // otherwise emit an empty agent turn. Only the audio path can reach
        // here with empty text — text/invoke/reaction empties were already
        // filtered by `parse_activity`.
        if inbound.text.trim().is_empty() {
            tracing::info!("teams: dropping voice-only message with no transcript");
            return Ok(WebhookOutcome::Skip);
        }

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

        // Teams replies via `reply_to_activity` already nest under the inbound
        // message; the prefix lets users visually distinguish /btw answers.
        let response_text = crate::btw_dispatch::maybe_prefix_btw(response_text, response_context);

        if let Err(e) = self
            .client
            .reply_to_activity(
                &ctx.service_url,
                &ctx.conversation_id,
                &ctx.activity_id,
                &response_text,
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
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn health() -> Arc<RwLock<ChannelHealthRegistry>> {
        Arc::new(RwLock::new(ChannelHealthRegistry::default()))
    }

    /// Channel wired to a loopback stub: token fetches and outbound sends
    /// both land on `server`.
    fn stub_channel(server: &MockServer, config: cortex::config::Config) -> TeamsChannel {
        TeamsChannel {
            client: Arc::new(
                TeamsClient::with_endpoints("app-1", "s3cret", &format!("{}/token", server.uri()))
                    .unwrap(),
            ),
            app_secret: Some(make_secret()),
            dedup: Arc::new(Mutex::new(ActivityDeduplicator::default())),
            config,
        }
    }

    async fn mount_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok-abc", "expires_in": 3600
            })))
            .mount(server)
            .await;
    }

    fn make_secret() -> String {
        base64::engine::general_purpose::STANDARD.encode(b"test-secret-key-32bytes-long!!")
    }

    fn make_signed_headers(secret_b64: &str, body: &str) -> HeaderMap {
        let secret_bytes = base64::engine::general_purpose::STANDARD
            .decode(secret_b64)
            .unwrap();
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&secret_bytes).expect("HMAC accepts any key size");
        mac.update(body.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            axum::http::HeaderValue::from_str(&format!("HMAC {sig}")).unwrap(),
        );
        headers
    }

    #[test]
    fn names_registers_the_teams_channel() {
        assert_eq!(channel(None).names(), vec!["teams"]);
    }

    #[tokio::test]
    async fn send_response_replies_to_the_activity_and_records_outbound() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v3/conversations/conv-1/activities/act-1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let channel = stub_channel(&server, cortex::config::Config::default());
        let health = health();
        let ctx = serde_json::json!({
            "service_url": server.uri(),
            "conversation_id": "conv-1",
            "activity_id": "act-1",
        });

        channel
            .send_response("hello there", &ctx, &health)
            .await
            .expect("send_response must succeed on 200");

        let snap = health.read().await.snapshot();
        let row = snap.iter().find(|s| s.name == "teams").expect("registered");
        assert_eq!(row.outbound_count, 1);
        assert_eq!(row.error_count, 0);
    }

    #[tokio::test]
    async fn send_response_api_failure_errs_and_records_error() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v3/conversations/conv-1/activities/act-1"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let channel = stub_channel(&server, cortex::config::Config::default());
        let health = health();
        let ctx = serde_json::json!({
            "service_url": server.uri(),
            "conversation_id": "conv-1",
            "activity_id": "act-1",
        });

        channel
            .send_response("hi", &ctx, &health)
            .await
            .expect_err("500 must propagate as an error");

        let snap = health.read().await.snapshot();
        let row = snap.iter().find(|s| s.name == "teams").expect("registered");
        assert_eq!(row.error_count, 1);
        assert_eq!(row.outbound_count, 0);
    }

    fn stt_config() -> cortex::config::Config {
        // A transcriber must exist for the attachment loop to run at all.
        // Key comes from a test-unique env var so no real credentials leak in.
        unsafe { std::env::set_var("BORG_TEST_TEAMS_STT_KEY", "test-key") };
        let mut config = cortex::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "openai".to_string(),
            model: None,
            api_key_env: Some("BORG_TEST_TEAMS_STT_KEY".to_string()),
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
        // A voice message carries one audio and one image attachment. Only
        // the audio one goes through download + STT; the image must be
        // skipped. The stub 500s the download so the failure placeholder
        // (not a live STT call) is the observable.
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let channel = stub_channel(&server, stt_config());

        let body = serde_json::json!({
            "id": "act-9",
            "type": "message",
            "from": {"id": "user-1", "name": "Alice"},
            "conversation": {"id": "conv-1"},
            "recipient": {"id": "bot-1", "name": "MyBot"},
            "serviceUrl": "https://smba.trafficmanager.net/teams/",
            "attachments": [
                {
                    "contentType": "audio/ogg",
                    "contentUrl": format!("{}/files/voice.ogg", server.uri()),
                    "name": "voice.ogg"
                },
                {
                    "contentType": "image/png",
                    "contentUrl": format!("{}/files/pic.png", server.uri()),
                    "name": "pic.png"
                }
            ]
        })
        .to_string();
        let headers = make_signed_headers(&make_secret(), &body);

        let gw_config = cortex::config::Config::default();
        let hreg = health();
        let ctx = WebhookContext {
            config: &gw_config,
            health: &hreg,
        };
        let outcome = channel
            .handle_webhook(&headers, &body, &ctx)
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
        let fetches: Vec<_> = reqs
            .iter()
            .filter(|r| r.url.path().starts_with("/files/"))
            .collect();
        assert_eq!(fetches.len(), 1, "exactly the audio attachment is fetched");
        assert_eq!(fetches[0].url.path(), "/files/voice.ogg");
    }

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

    fn channel(app_secret: Option<&str>) -> TeamsChannel {
        TeamsChannel {
            client: Arc::new(super::TeamsClient::new("app-id", "app-secret").unwrap()),
            app_secret: app_secret.map(str::to_string),
            dedup: Arc::new(Mutex::new(ActivityDeduplicator::default())),
            config: cortex::config::Config::default(),
        }
    }

    /// Every field Teams sends — including who the message is from — is
    /// attacker-controlled JSON until the signature proves otherwise. With no
    /// app secret configured the old code skipped verification and processed
    /// the activity anyway, so anyone who found the URL could drive the agent
    /// as the owner. Refuse instead, and say which credential is missing.
    #[tokio::test]
    async fn an_unconfigured_app_secret_refuses_the_activity() {
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::default()));
        let config = cortex::config::Config::default();
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        let activity = r#"{"type":"message","text":"whoami","from":{"id":"attacker"},
            "conversation":{"id":"c1"},"serviceUrl":"https://evil.example"}"#;

        let msg = match channel(None)
            .handle_webhook(&HeaderMap::new(), activity, &ctx)
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unsigned activity must be refused when no secret is set"),
        };
        assert!(
            msg.contains("TEAMS_APP_SECRET"),
            "the refusal must name the missing credential: {msg}"
        );

        // A blank secret is the same hole wearing a value.
        assert!(
            channel(Some("   "))
                .handle_webhook(&HeaderMap::new(), activity, &ctx)
                .await
                .is_err(),
            "a blank app secret must not count as configured"
        );

        // Configured but wrong/absent signature: still refused, now by the
        // signature check rather than the configuration guard.
        assert!(
            channel(Some("s3cr3t"))
                .handle_webhook(&HeaderMap::new(), activity, &ctx)
                .await
                .is_err(),
            "an unsigned activity must fail verification"
        );
    }
}
