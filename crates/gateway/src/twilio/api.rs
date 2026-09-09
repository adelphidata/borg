use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use std::time::Duration;

use crate::constants::{GATEWAY_HTTP_TIMEOUT, TWILIO_API_BASE};
use crate::twilio::types::WHATSAPP_PREFIX;

fn with_whatsapp_prefix(number: &str) -> String {
    if number.starts_with(WHATSAPP_PREFIX) {
        number.to_string()
    } else {
        format!("{WHATSAPP_PREFIX}{number}")
    }
}

/// Client for sending messages via the Twilio REST API.
pub struct TwilioClient {
    client: Client,
    account_sid: String,
    auth_header: String,
    base_url: String,
}

impl TwilioClient {
    pub fn new(account_sid: &str, auth_token: &str) -> Result<Self> {
        Self::with_base(account_sid, auth_token, TWILIO_API_BASE)
    }

    /// Build a client against a custom API base URL (tests point this at a
    /// loopback stub server).
    pub(crate) fn with_base(account_sid: &str, auth_token: &str, base_url: &str) -> Result<Self> {
        let credentials = format!("{account_sid}:{auth_token}");
        let auth_header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        );

        Ok(Self {
            client: crate::resilience::http_retry::build_channel_client(
                "Twilio",
                GATEWAY_HTTP_TIMEOUT,
                Some(Duration::from_secs(10)),
            )?,
            account_sid: account_sid.to_string(),
            auth_header,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Send an SMS message.
    pub async fn send_sms(&self, from: &str, to: &str, body: &str) -> Result<String> {
        // SMS is plain-text only — strip any outer markdown code fence so
        // preformatted command output (e.g. /xp, /card, /evolution) doesn't
        // arrive with literal backticks. WhatsApp supports ```monospace```
        // so `send_whatsapp` does NOT strip.
        let stripped = crate::message_processing::plain_text::strip_outer_code_fence(body);
        self.send_message(from, to, &stripped).await
    }

    /// Send a WhatsApp message, applying the `whatsapp:` address prefix
    /// idempotently so a caller may pass either form.
    pub async fn send_whatsapp(&self, from: &str, to: &str, body: &str) -> Result<String> {
        let from = with_whatsapp_prefix(from);
        let to = with_whatsapp_prefix(to);
        self.send_message(&from, &to, body).await
    }

    /// Send a message via the Twilio Messages API.
    async fn send_message(&self, from: &str, to: &str, body: &str) -> Result<String> {
        let url = format!(
            "{}/Accounts/{}/Messages.json",
            self.base_url, self.account_sid
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", &self.auth_header)
            .form(&[("From", from), ("To", to), ("Body", body)])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Twilio API error ({status}): {text}");
        }

        let json: serde_json::Value = response.json().await?;
        let sid = json["sid"].as_str().unwrap_or("unknown").to_string();

        Ok(sid)
    }

    /// Send a message with a media URL attachment.
    pub async fn send_message_with_media(
        &self,
        from: &str,
        to: &str,
        body: &str,
        media_url: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/Accounts/{}/Messages.json",
            self.base_url, self.account_sid
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", &self.auth_header)
            .form(&[
                ("From", from),
                ("To", to),
                ("Body", body),
                ("MediaUrl", media_url),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Twilio API error ({status}): {text}");
        }

        let json: serde_json::Value = response.json().await?;
        let sid = json["sid"].as_str().unwrap_or("unknown").to_string();

        Ok(sid)
    }

    /// Download media bytes from a Twilio media URL using the client's
    /// existing Basic-auth credentials. Used to fetch voice/audio
    /// attachments for STT before handing them to the agent.
    pub async fn download_media(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(url)
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .context("twilio: GET media url")?;
        if !resp.status().is_success() {
            anyhow::bail!("twilio media download returned HTTP {}", resp.status());
        }
        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk while streaming.
        cortex::media::read_body_capped(
            resp,
            crate::constants::ATTACHMENT_DOWNLOAD_MAX_BYTES,
            "Twilio media",
        )
        .await
        .context("twilio: read media body")
    }

    /// Returns the Messages API URL (for testing).
    pub fn messages_url(&self) -> String {
        format!(
            "{}/Accounts/{}/Messages.json",
            self.base_url, self.account_sid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn expected_auth(sid: &str, token: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{sid}:{token}"))
        )
    }

    #[test]
    fn whatsapp_prefix_applied_idempotently() {
        // The prefix is the WhatsApp address form; doubling it or dropping
        // it makes Twilio reject the To/From number.
        assert_eq!(
            with_whatsapp_prefix("+15551234567"),
            "whatsapp:+15551234567"
        );
        assert_eq!(
            with_whatsapp_prefix("whatsapp:+15551234567"),
            "whatsapp:+15551234567"
        );
    }

    #[tokio::test]
    async fn send_sms_posts_form_and_returns_api_sid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Accounts/AC123/Messages.json"))
            .and(header("Authorization", expected_auth("AC123", "tok")))
            .and(body_string_contains("From=%2B15550001111"))
            .and(body_string_contains("To=%2B15552223333"))
            .and(body_string_contains("Body=hello+from+borg"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "SM_sms_1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let sid = client
            .send_sms("+15550001111", "+15552223333", "hello from borg")
            .await
            .expect("2xx response with sid must succeed");
        assert_eq!(sid, "SM_sms_1", "sid must come from the API response");
    }

    #[tokio::test]
    async fn send_sms_strips_outer_code_fence_for_plain_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "SM_f"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        client
            .send_sms("+15550001111", "+15552223333", "```\nfenced output\n```")
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body).to_string();
        assert!(
            body.contains("Body=fenced+output"),
            "SMS body must be the fence contents: {body}"
        );
        assert!(
            !body.contains("%60"),
            "literal backticks must not reach an SMS: {body}"
        );
    }

    #[tokio::test]
    async fn send_whatsapp_prefixes_addresses_and_returns_api_sid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Accounts/AC123/Messages.json"))
            .and(body_string_contains("From=whatsapp%3A%2B15550001111"))
            .and(body_string_contains("To=whatsapp%3A%2B15552223333"))
            .and(body_string_contains("Body=hi+there"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "SM_wa_1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let sid = client
            .send_whatsapp("+15550001111", "+15552223333", "hi there")
            .await
            .expect("2xx response with sid must succeed");
        assert_eq!(sid, "SM_wa_1", "sid must come from the API response");
    }

    #[tokio::test]
    async fn send_message_surfaces_api_error_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid To number"))
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let err = client
            .send_sms("+1555", "+1666", "x")
            .await
            .expect_err("a 4xx must not be reported as sent");
        let msg = format!("{err}");
        assert!(msg.contains("400"), "error must carry the status: {msg}");
        assert!(
            msg.contains("invalid To number"),
            "error must carry the API body: {msg}"
        );
    }

    #[tokio::test]
    async fn send_message_with_media_posts_media_url_and_returns_api_sid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Accounts/AC123/Messages.json"))
            .and(body_string_contains("From=%2B15550001111"))
            .and(body_string_contains("To=%2B15552223333"))
            .and(body_string_contains(
                "MediaUrl=https%3A%2F%2Fexample.com%2Fcat.png",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"sid": "MM_1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let sid = client
            .send_message_with_media(
                "+15550001111",
                "+15552223333",
                "see attached",
                "https://example.com/cat.png",
            )
            .await
            .expect("2xx response with sid must succeed");
        assert_eq!(sid, "MM_1", "sid must come from the API response");
    }

    #[tokio::test]
    async fn download_media_returns_exact_bytes_with_basic_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media/ME123"))
            .and(header("Authorization", expected_auth("AC123", "tok")))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"twilio-media-payload".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let bytes = client
            .download_media(&format!("{}/media/ME123", server.uri()))
            .await
            .expect("2xx media download must succeed");
        assert_eq!(
            bytes, b"twilio-media-payload",
            "downloaded bytes must be the response body, unaltered"
        );
    }

    #[tokio::test]
    async fn download_media_rejects_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("gone"))
            .mount(&server)
            .await;

        let client = TwilioClient::with_base("AC123", "tok", &server.uri()).unwrap();
        let err = client
            .download_media(&format!("{}/media/ME404", server.uri()))
            .await
            .expect_err("a 404 body must not be handed to STT as audio");
        assert!(
            format!("{err:#}").contains("404"),
            "error must carry the status: {err:#}"
        );
    }

    #[test]
    fn api_url_construction() {
        let client = TwilioClient::new("AC123", "auth-token").unwrap();
        assert_eq!(
            client.messages_url(),
            "https://api.twilio.com/2010-04-01/Accounts/AC123/Messages.json"
        );
    }

    #[test]
    fn basic_auth_header() {
        let client = TwilioClient::new("AC123", "mytoken").unwrap();
        let expected_creds = base64::engine::general_purpose::STANDARD.encode("AC123:mytoken");
        assert_eq!(client.auth_header, format!("Basic {expected_creds}"));
    }

    #[test]
    fn messages_url_includes_account_sid() {
        let client = TwilioClient::new("AC_DIFFERENT_SID", "token").unwrap();
        let url = client.messages_url();
        assert!(url.contains("AC_DIFFERENT_SID"));
        assert_eq!(
            url,
            "https://api.twilio.com/2010-04-01/Accounts/AC_DIFFERENT_SID/Messages.json"
        );
    }
}
