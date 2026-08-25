//! `NativeChannel` implementation for Twilio (SMS + WhatsApp).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use cortex::config::Config;

use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

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
        // Fail closed: an unverified Twilio POST carries an attacker-chosen
        // `From`, which is the whole sender identity, so a missing auth token
        // must refuse rather than skip the check.
        crate::verify_common::require_verification_secret(
            "twilio",
            self.auth_token.as_deref(),
            "TWILIO_AUTH_TOKEN",
        )?;
        // Non-empty past the check above; an empty token would only make the
        // signature comparison fail, which stays closed either way.
        let auth_token = self.auth_token.as_deref().unwrap_or_default();
        // Twilio signs the full request URL, so the public URL is part of the
        // credential here rather than a convenience setting.
        let Some(ref public_url) = self.config.gateway.public_url else {
            anyhow::bail!(
                "Twilio inbound webhook refused: gateway.public_url is not set, and \
                 Twilio's signature covers the request URL — set it to the address \
                 Twilio posts to (failing closed)."
            )
        };
        let webhook_url = format!("{public_url}/webhook/twilio");
        crate::twilio::verify::verify_twilio_signature(headers, &webhook_url, body, auth_token)?;

        let mut parsed = super::parse::parse_webhook(body)?;

        // Inbound voice/audio transcription — parity with the Telegram
        // path (gateway/src/telegram/mod.rs:72-115). When the message
        // carries an audio attachment AND the STT pipeline is
        // configured, download via Twilio's Basic auth + transcribe +
        // inject as `[Voice transcript]:` text. Download failure is logged
        // and surfaced as a `[… could not be downloaded]` placeholder line
        // rather than dropping the turn.
        if let (Some(audio_url), Some(audio_mime)) =
            (parsed.audio_url.clone(), parsed.audio_mime.clone())
        {
            if let Some(transcriber) =
                cortex::media::understanding::AudioTranscriber::from_config(&self.config)
            {
                match self.client.download_media(&audio_url).await {
                    Ok(bytes) => {
                        let filename = format!(
                            "twilio-{}.audio",
                            parsed.message.message_id.as_deref().unwrap_or("unknown")
                        );
                        if let Some(transcript) = voice::transcribe_or_warn(
                            &transcriber,
                            &bytes,
                            &audio_mime,
                            &filename,
                            "twilio",
                        )
                        .await
                        {
                            parsed.message.text =
                                format!("{}{transcript}", voice::VOICE_TRANSCRIPT_PREFIX);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "twilio: media download failed");
                        crate::message_processing::media_failure::append_download_failure(
                            &mut parsed.message.text,
                            crate::message_processing::media_failure::kind_for_mime(&audio_mime),
                        );
                    }
                }
            }
        }

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
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The channel registry and webhook routing key on these exact names;
    /// dropping one silently unbinds `[[gateway.bindings]]` rows for it.
    #[test]
    fn names_cover_twilio_whatsapp_and_sms() {
        assert_eq!(
            channel(None, None).names(),
            vec!["twilio", "whatsapp", "sms"]
        );
    }

    fn wired_channel(server_uri: &str) -> TwilioChannel {
        TwilioChannel {
            client: Arc::new(TwilioClient::with_base("AC-sid", "tok", server_uri).unwrap()),
            auth_token: None,
            phone_number: Some("+15550000000".to_string()),
            whatsapp_number: Some("+15551111111".to_string()),
            config: Config::default(),
        }
    }

    /// `send_response` must actually deliver via the Messages API and record
    /// the outbound event — a silent Ok(()) would drop replies while the
    /// agent believes they were sent.
    #[tokio::test]
    async fn send_response_sms_delivers_and_records_outbound() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Accounts/AC-sid/Messages.json"))
            .and(body_string_contains("From=%2B15550000000"))
            .and(body_string_contains("To=%2B15559990000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "SM_ch_1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let channel = wired_channel(&server.uri());
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::default()));
        let ctx = serde_json::json!({
            "channel_type": "sms",
            "sender": "+15559990000",
            "from_number": "+15550000000",
        });
        channel
            .send_response("hello", &ctx, &health)
            .await
            .expect("delivered response must be Ok");

        let body =
            String::from_utf8_lossy(&server.received_requests().await.unwrap()[0].body).to_string();
        assert!(
            !body.contains("whatsapp"),
            "sms context must use the plain SMS path: {body}"
        );
        let snapshot = health.read().await.snapshot();
        let sms = snapshot.iter().find(|s| s.name == "sms").expect("sms row");
        assert_eq!(sms.outbound_count, 1, "outbound event must be recorded");
        assert_eq!(sms.error_count, 0);
    }

    /// A whatsapp context must route through the WhatsApp sender, which
    /// applies the `whatsapp:` address prefix. Inverting the branch would
    /// send WhatsApp replies as SMS to an unprefixed number.
    #[tokio::test]
    async fn send_response_whatsapp_routes_through_whatsapp_sender() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Accounts/AC-sid/Messages.json"))
            .and(body_string_contains("From=whatsapp%3A%2B15551111111"))
            .and(body_string_contains("To=whatsapp%3A%2B15559990000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "SM_ch_2"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let channel = wired_channel(&server.uri());
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::default()));
        let ctx = serde_json::json!({
            "channel_type": "whatsapp",
            "sender": "+15559990000",
            "from_number": "+15551111111",
        });
        channel
            .send_response("hello", &ctx, &health)
            .await
            .expect("delivered response must be Ok");

        let snapshot = health.read().await.snapshot();
        let wa = snapshot
            .iter()
            .find(|s| s.name == "whatsapp")
            .expect("whatsapp row");
        assert_eq!(wa.outbound_count, 1, "outbound event must be recorded");
    }

    /// A failed send must surface the error AND record it against the
    /// channel's health — reporting success on failure is forbidden.
    #[tokio::test]
    async fn send_response_failure_is_err_and_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream broke"))
            .mount(&server)
            .await;

        let channel = wired_channel(&server.uri());
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::default()));
        let ctx = serde_json::json!({
            "channel_type": "sms",
            "sender": "+15559990000",
            "from_number": "+15550000000",
        });
        channel
            .send_response("hello", &ctx, &health)
            .await
            .expect_err("a 5xx from Twilio must not be reported as sent");

        let snapshot = health.read().await.snapshot();
        let sms = snapshot.iter().find(|s| s.name == "sms").expect("sms row");
        assert_eq!(sms.error_count, 1, "error event must be recorded");
        assert_eq!(sms.outbound_count, 0);
    }

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

    fn channel(auth_token: Option<&str>, public_url: Option<&str>) -> TwilioChannel {
        let mut config = Config::default();
        config.gateway.public_url = public_url.map(str::to_string);
        TwilioChannel {
            client: Arc::new(super::TwilioClient::new("AC-sid", "token").unwrap()),
            auth_token: auth_token.map(str::to_string),
            phone_number: Some("+15550000000".to_string()),
            whatsapp_number: None,
            config,
        }
    }

    /// The `From` number in a Twilio POST is the entire sender identity, and
    /// Borg routes per sender. Without the auth token the signature was simply
    /// not checked, so anyone posting to the endpoint could claim any number.
    /// Both halves of the credential are required, because Twilio signs the
    /// request URL as well as the body.
    #[tokio::test]
    async fn twilio_refuses_inbound_when_it_cannot_verify_the_signature() {
        let health = Arc::new(RwLock::new(crate::health::ChannelHealthRegistry::default()));
        let config = Config::default();
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        let body = "From=%2B15551234567&Body=whoami";

        // No auth token: refused, naming the missing credential.
        let msg = match channel(None, Some("https://borg.example"))
            .handle_webhook(&HeaderMap::new(), body, &ctx)
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unverifiable Twilio POST must be refused"),
        };
        assert!(
            msg.contains("TWILIO_AUTH_TOKEN"),
            "the refusal must name the missing credential: {msg}"
        );

        // Token present but no public URL: the signature covers the URL, so
        // this is equally unverifiable and must not fall through to accepting.
        let msg = match channel(Some("tok"), None)
            .handle_webhook(&HeaderMap::new(), body, &ctx)
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a missing public_url must not skip verification"),
        };
        assert!(
            msg.contains("public_url"),
            "the refusal must name what to configure: {msg}"
        );

        // Fully configured but unsigned: refused by the signature check.
        assert!(
            channel(Some("tok"), Some("https://borg.example"))
                .handle_webhook(&HeaderMap::new(), body, &ctx)
                .await
                .is_err(),
            "an unsigned request must fail verification"
        );
    }
}
