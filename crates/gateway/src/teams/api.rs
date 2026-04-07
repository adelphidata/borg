use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use tracing::warn;

use super::types::{ReplyActivity, TokenResponse};
use crate::chunker;
use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT};
use crate::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";
/// Buffer in seconds before token expiry to trigger refresh.
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;

/// Known valid Microsoft service URL host suffixes for SSRF protection.
const ALLOWED_HOST_SUFFIXES: &[&str] = &[".botframework.com", ".teams.microsoft.com", ".skype.com"];

/// Cached OAuth2 token with expiry tracking.
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// A client for the Microsoft Bot Framework / Teams API.
pub struct TeamsClient {
    client: Client,
    app_id: String,
    app_secret: String,
    token_cache: RwLock<Option<CachedToken>>,
}

impl TeamsClient {
    pub fn new(app_id: &str, app_secret: &str) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(GATEWAY_HTTP_TIMEOUT)
                .build()
                .context("Failed to build Teams HTTP client")?,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token_cache: RwLock::new(None),
        })
    }

    /// Get a valid OAuth2 access token, fetching a new one if the cache is empty or expired.
    pub async fn get_token(&self) -> Result<String> {
        // Check cache first
        {
            let cache = self
                .token_cache
                .read()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
            if let Some(ref cached) = *cache {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Fetch a new token
        let resp = self
            .client
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.app_id),
                ("client_secret", &self.app_secret),
                ("scope", "https://api.botframework.com/.default"),
            ])
            .send()
            .await
            .context("Failed to request Teams OAuth2 token")?;

        let status = resp.status();
        if !status.is_success() {
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("Failed to read Teams token error response body: {e}");
                    String::new()
                }
            };
            bail!("Token request failed ({status}): {body}");
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse token response")?;

        let expires_at = Instant::now()
            + Duration::from_secs(
                token_resp
                    .expires_in
                    .saturating_sub(TOKEN_EXPIRY_BUFFER_SECS),
            );

        let access_token = token_resp.access_token.clone();

        // Update cache
        {
            let mut cache = self
                .token_cache
                .write()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {e}"))?;
            *cache = Some(CachedToken {
                access_token: token_resp.access_token,
                expires_at,
            });
        }

        Ok(access_token)
    }

    /// Reply to a specific activity in a Teams conversation, automatically chunking long messages.
    pub async fn reply_to_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        text: &str,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);

        let chunks = chunker::chunk_text_nonempty(text, DEFAULT_MESSAGE_CHUNK_SIZE);

        for chunk in &chunks {
            let url = format!("{base}v3/conversations/{conversation_id}/activities/{activity_id}");
            let reply = ReplyActivity::message(chunk.as_str());
            self.send_with_retry(&url, &reply).await?;
        }

        Ok(())
    }

    /// Send a message to a Teams conversation (not a reply to a specific activity).
    /// Automatically chunks long messages.
    pub async fn send_to_conversation(
        &self,
        service_url: &str,
        conversation_id: &str,
        text: &str,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);

        let chunks = chunker::chunk_text_nonempty(text, DEFAULT_MESSAGE_CHUNK_SIZE);

        for chunk in &chunks {
            let url = format!("{base}v3/conversations/{conversation_id}/activities");
            let reply = ReplyActivity::message(chunk.as_str());
            self.send_with_retry(&url, &reply).await?;
        }

        Ok(())
    }

    /// Send a typing indicator to a conversation.
    pub async fn send_typing(&self, service_url: &str, conversation_id: &str) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = format!("{base}v3/conversations/{conversation_id}/activities");
        let typing = ReplyActivity::typing();
        self.send_with_retry(&url, &typing).await
    }

    /// Send an arbitrary activity to a conversation (used by streaming).
    pub async fn send_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity: &ReplyActivity,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = format!("{base}v3/conversations/{conversation_id}/activities");
        self.send_with_retry(&url, activity).await
    }

    /// Send an activity as a reply to a specific activity (used by streaming finalize).
    pub async fn send_reply_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        activity: &ReplyActivity,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = format!("{base}v3/conversations/{conversation_id}/activities/{activity_id}");
        self.send_with_retry(&url, activity).await
    }

    /// Edit (update) an existing activity in a conversation.
    pub async fn edit_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        new_text: &str,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = format!("{base}v3/conversations/{conversation_id}/activities/{activity_id}");
        let body = ReplyActivity::message(new_text);
        let token = self.get_token().await?;
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("Failed to edit Teams activity")?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok());
        let kind = super::errors::classify_status(status.as_u16(), retry_after);
        let hint = super::errors::error_hint(&kind);
        let error_body = resp.text().await.unwrap_or_default();
        bail!("Teams edit_activity failed ({status}): {hint} — {error_body}");
    }

    /// Delete an activity from a conversation.
    pub async fn delete_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
    ) -> Result<()> {
        validate_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = format!("{base}v3/conversations/{conversation_id}/activities/{activity_id}");
        let token = self.get_token().await?;
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Failed to delete Teams activity")?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok());
        let kind = super::errors::classify_status(status.as_u16(), retry_after);
        let hint = super::errors::error_hint(&kind);
        let error_body = resp.text().await.unwrap_or_default();
        bail!("Teams delete_activity failed ({status}): {hint} — {error_body}");
    }

    /// Send a single request with 429 retry logic and error classification.
    async fn send_with_retry(&self, url: &str, body: &ReplyActivity) -> Result<()> {
        let policy = RateLimitPolicy {
            service_name: "Teams",
            ..RateLimitPolicy::default()
        };

        let resp = send_with_rate_limit_retry(&policy, || async {
            let token = self.get_token().await?;
            self.client
                .post(url)
                .bearer_auth(&token)
                .json(body)
                .send()
                .await
                .context("Failed to send Teams message")
        })
        .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok());
        let kind = super::errors::classify_status(status.as_u16(), retry_after);
        let hint = super::errors::error_hint(&kind);
        let error_body = resp.text().await.unwrap_or_default();
        if let super::errors::TeamsErrorKind::Throttled { retry_after_secs } = &kind {
            bail!(
                "Teams API rate limited ({status}): retry after {retry_after_secs}s — {error_body}"
            );
        }
        bail!("Teams API request failed ({status}): {hint} — {error_body}");
    }
}

/// Validate that a service URL is a legitimate Microsoft Bot Framework endpoint.
/// Prevents SSRF by requiring HTTPS and a known Microsoft host suffix.
fn validate_service_url(url: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("Invalid service URL: {e}"))?;

    if parsed.scheme() != "https" {
        bail!("Service URL must use HTTPS: {url}");
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Service URL has no host: {url}"))?;

    let is_allowed = ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix));

    if !is_allowed {
        bail!("Service URL host not allowed: {host}");
    }

    Ok(())
}

/// Ensure a URL has a trailing slash for path concatenation.
fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_construction_reply() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams/");
        let url = format!(
            "{base}v3/conversations/{conv}/activities/{act}",
            conv = "c1",
            act = "a1"
        );
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/c1/activities/a1"
        );
    }

    #[test]
    fn url_construction_send() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams");
        let url = format!("{base}v3/conversations/{conv}/activities", conv = "c1");
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/c1/activities"
        );
    }

    #[test]
    fn validate_service_url_valid_botframework() {
        assert!(validate_service_url("https://smba.trafficmanager.net.botframework.com/").is_ok());
    }

    #[test]
    fn validate_service_url_valid_teams() {
        assert!(validate_service_url("https://api.teams.microsoft.com/").is_ok());
    }

    #[test]
    fn validate_service_url_valid_skype() {
        assert!(validate_service_url("https://smba.skype.com/teams/").is_ok());
    }

    #[test]
    fn validate_service_url_rejects_http() {
        let result = validate_service_url("http://smba.trafficmanager.net.botframework.com/");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HTTPS"));
    }

    #[test]
    fn validate_service_url_rejects_unknown_host() {
        let result = validate_service_url("https://evil.example.com/");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not allowed"));
    }

    #[test]
    fn validate_service_url_rejects_invalid_url() {
        let result = validate_service_url("not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn trailing_slash_added() {
        assert_eq!(
            ensure_trailing_slash("https://example.com"),
            "https://example.com/"
        );
    }

    #[test]
    fn trailing_slash_not_doubled() {
        assert_eq!(
            ensure_trailing_slash("https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn chunking_integration() {
        let long_text: String = "a".repeat(8500);
        let chunks = chunker::chunk_text(&long_text, DEFAULT_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 4000);
        assert_eq!(chunks[1].len(), 4000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn token_endpoint_url() {
        assert_eq!(
            TOKEN_ENDPOINT,
            "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn typing_activity_url_construction() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams/");
        let url = format!("{base}v3/conversations/{conv}/activities", conv = "c1");
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/c1/activities"
        );
    }

    #[test]
    fn typing_activity_serialization() {
        let typing = super::super::types::ReplyActivity::typing();
        let json = serde_json::to_value(&typing).unwrap();
        assert_eq!(json["type"], "typing");
    }

    #[test]
    fn url_construction_edit_activity() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams/");
        let url = format!(
            "{base}v3/conversations/{conv}/activities/{act}",
            conv = "conv-123",
            act = "act-456"
        );
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/conv-123/activities/act-456"
        );
    }

    #[test]
    fn url_construction_delete_activity() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams");
        let url = format!(
            "{base}v3/conversations/{conv}/activities/{act}",
            conv = "c1",
            act = "a1"
        );
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/c1/activities/a1"
        );
    }

    #[test]
    fn send_activity_url_construction() {
        let base = ensure_trailing_slash("https://smba.trafficmanager.net/teams/");
        let url = format!("{base}v3/conversations/{conv}/activities", conv = "conv-1");
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/teams/v3/conversations/conv-1/activities"
        );
    }
}
