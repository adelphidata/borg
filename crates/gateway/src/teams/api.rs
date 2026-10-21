use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use tracing::warn;

use super::types::{Mention, ReplyActivity, TokenResponse};
use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, GATEWAY_HTTP_TIMEOUT};
use crate::message_processing::chunker;
use crate::resilience::http_retry::{send_with_rate_limit_retry, RateLimitPolicy};

const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";
/// Buffer in seconds before token expiry to trigger refresh.
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;

/// Known valid Microsoft service URL host suffixes for SSRF protection.
const ALLOWED_HOST_SUFFIXES: &[&str] = &[".botframework.com", ".teams.microsoft.com", ".skype.com"];

/// Exact Microsoft hosts that carry no allowed suffix. `smba.trafficmanager.net`
/// is the `serviceUrl` host every Teams activity arrives with; the suffix
/// `.trafficmanager.net` itself is far too broad (any Azure Traffic Manager
/// profile), so only this one host is pinned.
const ALLOWED_HOSTS: &[&str] = &["smba.trafficmanager.net"];

/// True when `host` is a Microsoft Bot Framework host: an exact entry in
/// [`ALLOWED_HOSTS`] or under one of [`ALLOWED_HOST_SUFFIXES`].
fn is_microsoft_host(host: &str) -> bool {
    ALLOWED_HOSTS.contains(&host)
        || ALLOWED_HOST_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

/// Most redirects one attachment download follows.
const MAX_ATTACHMENT_REDIRECTS: usize = 3;

/// Token-scope guard for attachment downloads: true when the Bot Framework
/// bearer token may go to `url`.
///
/// Production (`test_base` is `None`): `https` on the default port and a
/// Microsoft host — an exact entry in [`ALLOWED_HOSTS`] or a host under one
/// of [`ALLOWED_HOST_SUFFIXES`] (see [`is_microsoft_host`]). The suffixes
/// carry a leading dot, so `evil-botframework.com` and
/// `botframework.com.evil.example` both fail. Under a test base: only that
/// base's scheme, host and port.
fn is_allowed_attachment_url(url: &reqwest::Url, test_base: Option<&reqwest::Url>) -> bool {
    match test_base {
        None => {
            url.scheme() == "https"
                && url.port_or_known_default() == Some(443)
                && url.host_str().is_some_and(is_microsoft_host)
        }
        Some(base) => {
            url.scheme() == base.scheme()
                && url.host_str() == base.host_str()
                && url.port_or_known_default() == base.port_or_known_default()
        }
    }
}

/// Cached OAuth2 token with expiry tracking.
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// A client for the Microsoft Bot Framework / Teams API.
pub struct TeamsClient {
    client: Client,
    /// Client for attachment downloads. It follows no redirect by itself, so
    /// `download_attachment` can check each hop before the token goes there.
    download_client: Client,
    app_id: String,
    app_secret: String,
    /// OAuth2 token endpoint. Production uses `TOKEN_ENDPOINT`; tests point
    /// this at a loopback stub server.
    token_endpoint: String,
    /// When true (always in production), service URLs must pass the
    /// Microsoft-host SSRF allowlist. Test-only stub clients relax this so
    /// they can target a loopback server.
    enforce_service_allowlist: bool,
    /// `None` in production (attachment downloads pinned to
    /// [`ALLOWED_HOST_SUFFIXES`]). A test stub pins downloads to the origin of
    /// its loopback token endpoint instead.
    attachment_test_base: Option<reqwest::Url>,
    token_cache: RwLock<Option<CachedToken>>,
}

impl TeamsClient {
    pub fn new(app_id: &str, app_secret: &str) -> Result<Self> {
        Self::build(app_id, app_secret, TOKEN_ENDPOINT, true)
    }

    /// Build a client whose OAuth token endpoint points at a loopback stub
    /// and whose service-URL allowlist is relaxed (tests only).
    #[cfg(test)]
    pub(crate) fn with_endpoints(
        app_id: &str,
        app_secret: &str,
        token_endpoint: &str,
    ) -> Result<Self> {
        Self::build(app_id, app_secret, token_endpoint, false)
    }

    fn build(
        app_id: &str,
        app_secret: &str,
        token_endpoint: &str,
        enforce_service_allowlist: bool,
    ) -> Result<Self> {
        let attachment_test_base = if enforce_service_allowlist {
            None
        } else {
            Some(reqwest::Url::parse(token_endpoint).context("Invalid Teams token endpoint")?)
        };
        let download_client = Client::builder()
            .timeout(GATEWAY_HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build Teams attachment download HTTP client")?;
        Ok(Self {
            client: crate::resilience::http_retry::build_channel_client(
                "Teams",
                GATEWAY_HTTP_TIMEOUT,
                None,
            )?,
            download_client,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token_endpoint: token_endpoint.to_string(),
            enforce_service_allowlist,
            attachment_test_base,
            token_cache: RwLock::new(None),
        })
    }

    /// SSRF gate for caller-supplied service URLs. Enforced on every public
    /// send/edit/delete method; only test stub clients bypass it.
    fn check_service_url(&self, url: &str) -> Result<()> {
        if self.enforce_service_allowlist {
            validate_service_url(url)?;
        }
        Ok(())
    }

    /// Get a valid OAuth2 access token, fetching a new one if the cache is empty or expired.
    pub async fn get_token(&self) -> Result<String> {
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

        let resp = self
            .client
            .post(&self.token_endpoint)
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

    /// Download an attachment from a Bot Framework content URL. Voice
    /// messages on Teams arrive as audio attachments; this method fetches
    /// the bytes for downstream STT. Uses the cached OAuth2 access token
    /// for authentication.
    ///
    /// The content URL is attacker-chosen JSON, so the token only goes to a
    /// Microsoft host over HTTPS. Redirects are followed by hand: each hop is
    /// checked against the same pin before the token goes there, and the
    /// chain is capped. A foreign host gets no request at all.
    pub async fn download_attachment(&self, url: &str) -> Result<Vec<u8>> {
        let mut current = reqwest::Url::parse(url).context("Invalid Teams attachment URL")?;
        let mut redirects = 0usize;
        let test_base = self.attachment_test_base.as_ref();
        if !is_allowed_attachment_url(&current, test_base) {
            bail!("Refusing to download Teams attachment from non-Microsoft host");
        }
        let token = self.get_token().await?;
        let resp = loop {
            if !is_allowed_attachment_url(&current, test_base) {
                bail!("Refusing to follow Teams attachment redirect to non-Microsoft host");
            }
            let resp = self
                .download_client
                .get(current.clone())
                .bearer_auth(&token)
                .send()
                .await
                .context("teams: download attachment")?;
            if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                break resp;
            }
            redirects += 1;
            if redirects > MAX_ATTACHMENT_REDIRECTS {
                bail!("Too many Teams attachment redirects");
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Teams attachment redirect has no Location header")?
                .to_str()
                .context("Teams attachment redirect Location is not valid text")?;
            current = current
                .join(location)
                .context("Teams attachment redirect Location is not a valid URL")?;
        };
        if !resp.status().is_success() {
            anyhow::bail!("teams attachment download returned HTTP {}", resp.status());
        }
        // Streamed read with a running cap — Content-Length can lie or be
        // absent, so the ceiling is enforced chunk-by-chunk while streaming.
        cortex::media::read_body_capped(
            resp,
            crate::constants::ATTACHMENT_DOWNLOAD_MAX_BYTES,
            "Teams attachment",
        )
        .await
        .context("teams: read attachment body")
    }

    /// Reply to a specific activity in a Teams conversation, automatically chunking long messages.
    pub async fn reply_to_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        text: &str,
    ) -> Result<()> {
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);

        let chunks = chunker::chunks_for_delivery(text, DEFAULT_MESSAGE_CHUNK_SIZE)?;

        for chunk in &chunks {
            let url = activities_url(&base, conversation_id, Some(activity_id));
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
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);

        let chunks = chunker::chunks_for_delivery(text, DEFAULT_MESSAGE_CHUNK_SIZE)?;

        for chunk in &chunks {
            let url = activities_url(&base, conversation_id, None);
            let reply = ReplyActivity::message(chunk.as_str());
            self.send_with_retry(&url, &reply).await?;
        }

        Ok(())
    }

    /// Reply to a specific activity, embedding @mentions.
    ///
    /// `text` should already contain `<at>Display Name</at>` tags at the spots
    /// where each mention appears; the matching `mention` entities are attached
    /// so Teams renders them as pills. When `mentions` is empty the call is
    /// equivalent to `reply_to_activity`. Long messages are still chunked, but
    /// only the **first** chunk carries the mention entities (subsequent chunks
    /// would re-ping the user, which is noisy).
    pub async fn reply_with_mentions(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        text: &str,
        mentions: &[Mention],
    ) -> Result<()> {
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);

        let chunks = chunker::chunks_for_delivery(text, DEFAULT_MESSAGE_CHUNK_SIZE)?;

        for (i, chunk) in chunks.iter().enumerate() {
            let url = activities_url(&base, conversation_id, Some(activity_id));
            let activity = if i == 0 {
                ReplyActivity::message_with_mentions(chunk.as_str(), mentions)
            } else {
                ReplyActivity::message(chunk.as_str())
            };
            self.send_with_retry(&url, &activity).await?;
        }

        Ok(())
    }

    /// Send a typing indicator to a conversation.
    pub async fn send_typing(&self, service_url: &str, conversation_id: &str) -> Result<()> {
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = activities_url(&base, conversation_id, None);
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
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = activities_url(&base, conversation_id, None);
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
        self.check_service_url(service_url)?;
        let base = ensure_trailing_slash(service_url);
        let url = activities_url(&base, conversation_id, Some(activity_id));
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
        self.check_service_url(service_url)?;
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
        self.check_service_url(service_url)?;
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
        let policy = RateLimitPolicy::for_service("Teams");

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

    if !is_microsoft_host(host) {
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

/// Build the Bot Framework REST path for a conversation's activities,
/// optionally scoped to one activity (reply/edit/delete) vs. the
/// conversation-level collection (send/typing). `base` must already have
/// a trailing slash (see `ensure_trailing_slash`).
fn activities_url(base: &str, conversation_id: &str, activity_id: Option<&str>) -> String {
    match activity_id {
        Some(id) => format!("{base}v3/conversations/{conversation_id}/activities/{id}"),
        None => format!("{base}v3/conversations/{conversation_id}/activities"),
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

    /// Real Teams activities carry `https://smba.trafficmanager.net/<region>/`
    /// as their `serviceUrl`; it has no allowed suffix, so it is pinned as an
    /// exact host. Sibling Traffic Manager profiles stay refused.
    #[test]
    fn validate_service_url_accepts_the_exact_teams_traffic_manager_host_only() {
        assert!(validate_service_url("https://smba.trafficmanager.net/amer/").is_ok());
        for host in [
            "evil.trafficmanager.net",
            "smba.trafficmanager.net.evil.example",
        ] {
            let result = validate_service_url(&format!("https://{host}/amer/"));
            assert!(result.is_err(), "{host} must be refused");
        }
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

    /// The bearer token may only travel to a Microsoft host over HTTPS on
    /// the default port. Look-alike hosts, plain HTTP and odd ports all fail
    /// the production pin.
    #[test]
    fn attachment_url_pin_accepts_only_https_microsoft_hosts() {
        let cases: &[(&str, bool)] = &[
            ("https://smba.skype.com/v1/objects/abc/views/audio", true),
            ("https://us-api.asm.skype.com/v1/objects/abc", true),
            ("https://api.botframework.com/v3/attachments/abc", true),
            ("https://api.teams.microsoft.com/files/abc", true),
            ("http://smba.skype.com/v1/objects/abc", false),
            ("https://smba.skype.com:8443/v1/objects/abc", false),
            ("https://evil.example.com/voice.ogg", false),
            ("https://skype.com.evil.example/voice.ogg", false),
            ("https://evil-botframework.com/voice.ogg", false),
            ("https://contoso.sharepoint.com/voice.ogg", false),
            ("https://127.0.0.1/voice.ogg", false),
        ];
        for (url, allowed) in cases {
            let parsed = reqwest::Url::parse(url).unwrap();
            assert_eq!(is_allowed_attachment_url(&parsed, None), *allowed, "{url}");
        }
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

    /// Every public send/edit/delete method takes a caller-supplied service
    /// URL and MUST run it through the SSRF allowlist before any network
    /// traffic. A method that silently succeeds against a non-Microsoft host
    /// is a data-exfiltration vector.
    #[tokio::test]
    async fn every_public_method_rejects_a_non_microsoft_service_url() {
        let client = TeamsClient::new("app-1", "s3cret").unwrap();
        let evil = "https://evil.example.com/";
        let msg = ReplyActivity::message("hi");
        let results: Vec<(&str, Result<()>)> = vec![
            (
                "reply_to_activity",
                client.reply_to_activity(evil, "c1", "a1", "hi").await,
            ),
            (
                "send_to_conversation",
                client.send_to_conversation(evil, "c1", "hi").await,
            ),
            (
                "reply_with_mentions",
                client
                    .reply_with_mentions(evil, "c1", "a1", "hi", &[])
                    .await,
            ),
            ("send_typing", client.send_typing(evil, "c1").await),
            (
                "send_activity",
                client.send_activity(evil, "c1", &msg).await,
            ),
            (
                "send_reply_activity",
                client.send_reply_activity(evil, "c1", "a1", &msg).await,
            ),
            (
                "edit_activity",
                client.edit_activity(evil, "c1", "a1", "hi").await,
            ),
            (
                "delete_activity",
                client.delete_activity(evil, "c1", "a1").await,
            ),
        ];
        for (name, result) in results {
            let err = result.expect_err(name);
            assert!(
                err.to_string().contains("not allowed"),
                "{name} must fail the allowlist check: {err}"
            );
        }
    }

    /// Loopback-stub tests: every Teams REST method must fetch a real OAuth
    /// token, issue the right HTTP verb/path/auth/payload, and surface API
    /// errors — a client that silently skips the request is broken.
    mod http_stub {
        use super::*;
        use wiremock::matchers::{body_json, body_string_contains, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn client_for(server: &MockServer) -> TeamsClient {
            TeamsClient::with_endpoints("app-1", "s3cret", &format!("{}/token", server.uri()))
                .unwrap()
        }

        /// Mount the OAuth stub: one long-lived token so send tests fetch
        /// exactly once and reuse the cache.
        async fn mount_token(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "tok-abc", "expires_in": 3600
                })))
                .mount(server)
                .await;
        }

        const BEARER: &str = "Bearer tok-abc";

        #[tokio::test]
        async fn get_token_posts_client_credentials_and_caches_the_token() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(body_string_contains("grant_type=client_credentials"))
                .and(body_string_contains("client_id=app-1"))
                .and(body_string_contains("client_secret=s3cret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "tok-abc", "expires_in": 3600
                })))
                .expect(1) // second call must come from the cache
                .mount(&server)
                .await;

            let client = client_for(&server);
            assert_eq!(client.get_token().await.unwrap(), "tok-abc");
            assert_eq!(client.get_token().await.unwrap(), "tok-abc");
        }

        #[tokio::test]
        async fn get_token_refetches_once_the_cached_token_expires() {
            let server = MockServer::start().await;
            // First token's lifetime equals the refresh buffer, so it is
            // already expired the moment it is cached.
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "tok-1", "expires_in": TOKEN_EXPIRY_BUFFER_SECS
                })))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "tok-2", "expires_in": 3600
                })))
                .mount(&server)
                .await;

            let client = client_for(&server);
            assert_eq!(client.get_token().await.unwrap(), "tok-1");
            assert_eq!(
                client.get_token().await.unwrap(),
                "tok-2",
                "an expired cached token must be replaced, not served stale"
            );
        }

        #[tokio::test]
        async fn get_token_error_status_reports_status_and_body() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(400).set_body_string("bad client"))
                .mount(&server)
                .await;

            let err = client_for(&server).get_token().await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("Token request failed")
                    && msg.contains("400")
                    && msg.contains("bad client"),
                "{msg}"
            );
        }

        #[tokio::test]
        async fn download_attachment_returns_the_bytes_with_bearer_auth() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("GET"))
                .and(path("/files/voice.ogg"))
                .and(header("authorization", BEARER))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8, 0, 9, 250]))
                .expect(1)
                .mount(&server)
                .await;

            let bytes = client_for(&server)
                .download_attachment(&format!("{}/files/voice.ogg", server.uri()))
                .await
                .unwrap();
            assert_eq!(bytes, vec![7u8, 0, 9, 250]);
        }

        /// A content URL on a host outside the pin gets no request at all —
        /// not even an unauthenticated one — and the download fails.
        #[tokio::test]
        async fn download_attachment_refuses_foreign_host_without_sending_token() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            let foreign = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8]))
                .expect(0)
                .mount(&foreign)
                .await;

            let err = client_for(&server)
                .download_attachment(&format!("{}/files/voice.ogg", foreign.uri()))
                .await
                .expect_err("foreign host must be refused");
            assert!(
                err.to_string().contains("non-Microsoft host"),
                "unexpected error: {err}"
            );
            assert!(
                foreign.received_requests().await.unwrap().is_empty(),
                "the foreign host must receive no request"
            );
        }

        /// A redirect from the pinned host to a foreign one is not followed:
        /// the token never reaches the redirect target.
        #[tokio::test]
        async fn download_attachment_refuses_redirect_to_foreign_host() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            let foreign = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8]))
                .expect(0)
                .mount(&foreign)
                .await;
            Mock::given(method("GET"))
                .and(path("/files/voice.ogg"))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("location", format!("{}/steal", foreign.uri())),
                )
                .mount(&server)
                .await;

            let err = client_for(&server)
                .download_attachment(&format!("{}/files/voice.ogg", server.uri()))
                .await
                .expect_err("foreign redirect must fail");
            assert!(
                err.to_string().contains("redirect to non-Microsoft host"),
                "unexpected error: {err}"
            );
            assert!(foreign.received_requests().await.unwrap().is_empty());
        }

        /// A redirect that stays on the pinned host is followed with the
        /// token still attached.
        #[tokio::test]
        async fn download_attachment_follows_same_host_redirect_with_token() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("GET"))
                .and(path("/files/voice.ogg"))
                .respond_with(
                    ResponseTemplate::new(302).insert_header("location", "/cdn/voice.ogg"),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/cdn/voice.ogg"))
                .and(header("authorization", BEARER))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![4u8, 2]))
                .expect(1)
                .mount(&server)
                .await;

            let bytes = client_for(&server)
                .download_attachment(&format!("{}/files/voice.ogg", server.uri()))
                .await
                .unwrap();
            assert_eq!(bytes, vec![4u8, 2]);
        }

        #[tokio::test]
        async fn download_attachment_http_error_is_an_error() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("GET"))
                .and(path("/files/missing.ogg"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;

            let err = client_for(&server)
                .download_attachment(&format!("{}/files/missing.ogg", server.uri()))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("404"), "{err}");
        }

        #[tokio::test]
        async fn reply_to_activity_posts_every_chunk_to_the_activity_path() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities/a1"))
                .and(header("authorization", BEARER))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;

            let long_text = "a".repeat(8500);
            client_for(&server)
                .reply_to_activity(&server.uri(), "c1", "a1", &long_text)
                .await
                .unwrap();

            let reqs = server.received_requests().await.unwrap();
            let bodies: Vec<serde_json::Value> = reqs
                .iter()
                .filter(|r| r.url.path() == "/v3/conversations/c1/activities/a1")
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect();
            assert!(bodies.len() > 1, "8500 chars must be chunked");
            for body in &bodies {
                assert_eq!(body["type"], "message");
            }
            let reassembled: String = bodies.iter().map(|b| b["text"].as_str().unwrap()).collect();
            assert_eq!(reassembled, long_text, "no chunk may be dropped");
        }

        #[tokio::test]
        async fn send_to_conversation_posts_to_the_conversation_collection() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c9/activities"))
                .and(header("authorization", BEARER))
                .and(body_json(
                    serde_json::json!({"type": "message", "text": "hello"}),
                ))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            client_for(&server)
                .send_to_conversation(&server.uri(), "c9", "hello")
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn reply_with_mentions_attaches_entities_only_to_the_first_chunk() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities/a1"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;

            let long_text = format!("<at>Alice</at> {}", "b".repeat(4500));
            let mentions = [Mention::new("29:alice", "Alice")];
            client_for(&server)
                .reply_with_mentions(&server.uri(), "c1", "a1", &long_text, &mentions)
                .await
                .unwrap();

            let reqs = server.received_requests().await.unwrap();
            let bodies: Vec<serde_json::Value> = reqs
                .iter()
                .filter(|r| r.url.path() == "/v3/conversations/c1/activities/a1")
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect();
            assert!(bodies.len() > 1, "text must span multiple chunks");
            assert_eq!(
                bodies[0]["entities"][0]["mentioned"]["id"], "29:alice",
                "first chunk carries the mention entities"
            );
            for later in &bodies[1..] {
                assert!(
                    later.get("entities").is_none(),
                    "later chunks must not re-ping the user: {later}"
                );
            }
        }

        #[tokio::test]
        async fn send_typing_posts_a_typing_activity() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities"))
                .and(body_json(serde_json::json!({"type": "typing", "text": ""})))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            client_for(&server)
                .send_typing(&server.uri(), "c1")
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn send_activity_and_send_reply_activity_hit_their_paths() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities"))
                .and(body_string_contains("plain send"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities/a7"))
                .and(body_string_contains("reply send"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let client = client_for(&server);
            client
                .send_activity(&server.uri(), "c1", &ReplyActivity::message("plain send"))
                .await
                .unwrap();
            client
                .send_reply_activity(
                    &server.uri(),
                    "c1",
                    "a7",
                    &ReplyActivity::message("reply send"),
                )
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn edit_activity_puts_the_new_text_and_surfaces_failures() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("PUT"))
                .and(path("/v3/conversations/c1/activities/a1"))
                .and(header("authorization", BEARER))
                .and(body_json(
                    serde_json::json!({"type": "message", "text": "edited"}),
                ))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/v3/conversations/c1/activities/gone"))
                .respond_with(ResponseTemplate::new(404).set_body_string("no such activity"))
                .mount(&server)
                .await;

            let client = client_for(&server);
            client
                .edit_activity(&server.uri(), "c1", "a1", "edited")
                .await
                .unwrap();

            let err = client
                .edit_activity(&server.uri(), "c1", "gone", "edited")
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("edit_activity failed")
                    && msg.contains("404")
                    && msg.contains("no such activity"),
                "{msg}"
            );
        }

        #[tokio::test]
        async fn delete_activity_sends_delete_and_surfaces_failures() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("DELETE"))
                .and(path("/v3/conversations/c1/activities/a1"))
                .and(header("authorization", BEARER))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path("/v3/conversations/c1/activities/locked"))
                .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
                .mount(&server)
                .await;

            let client = client_for(&server);
            client
                .delete_activity(&server.uri(), "c1", "a1")
                .await
                .unwrap();

            let err = client
                .delete_activity(&server.uri(), "c1", "locked")
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("delete_activity failed") && msg.contains("403"),
                "{msg}"
            );
        }

        #[tokio::test]
        async fn send_error_status_is_classified_and_propagated() {
            let server = MockServer::start().await;
            mount_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v3/conversations/c1/activities/a1"))
                .respond_with(ResponseTemplate::new(401).set_body_string("token rejected"))
                .mount(&server)
                .await;

            let err = client_for(&server)
                .reply_to_activity(&server.uri(), "c1", "a1", "hi")
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("Teams API request failed")
                    && msg.contains("401")
                    && msg.contains("token rejected"),
                "{msg}"
            );
        }
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
