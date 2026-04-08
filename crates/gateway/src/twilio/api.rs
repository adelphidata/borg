use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use std::time::Duration;

use crate::constants::GATEWAY_HTTP_TIMEOUT;

const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

/// Client for sending messages via the Twilio REST API.
pub struct TwilioClient {
    client: Client,
    account_sid: String,
    auth_header: String,
}

impl TwilioClient {
    pub fn new(account_sid: &str, auth_token: &str) -> Result<Self> {
        let credentials = format!("{account_sid}:{auth_token}");
        let auth_header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        );

        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .connect_timeout(Duration::from_secs(10))
                .build()
                .context("Failed to build Twilio HTTP client")?,
            account_sid: account_sid.to_string(),
            auth_header,
        })
    }

    /// Send an SMS message.
    pub async fn send_sms(&self, from: &str, to: &str, body: &str) -> Result<String> {
        self.send_message(from, to, body).await
    }

    /// Send a WhatsApp message.
    /// Prefixes numbers with "whatsapp:" if not already present.
    pub async fn send_whatsapp(&self, from: &str, to: &str, body: &str) -> Result<String> {
        let from = if from.starts_with("whatsapp:") {
            from.to_string()
        } else {
            format!("whatsapp:{from}")
        };
        let to = if to.starts_with("whatsapp:") {
            to.to_string()
        } else {
            format!("whatsapp:{to}")
        };
        self.send_message(&from, &to, body).await
    }

    /// Send a message via the Twilio Messages API.
    async fn send_message(&self, from: &str, to: &str, body: &str) -> Result<String> {
        let url = format!(
            "{}/Accounts/{}/Messages.json",
            TWILIO_API_BASE, self.account_sid
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
            TWILIO_API_BASE, self.account_sid
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

    /// Returns the Messages API URL (for testing).
    pub fn messages_url(&self) -> String {
        format!(
            "{}/Accounts/{}/Messages.json",
            TWILIO_API_BASE, self.account_sid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
