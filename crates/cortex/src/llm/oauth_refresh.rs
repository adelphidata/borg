//! OAuth access-token refresh for pooled OAuth credentials.
//!
//! Each refresh function takes a refresh token and returns a fresh
//! `(access_token, refresh_token, expires_at_ms)` triple. Refresh tokens
//! rotate on every successful refresh — callers must persist the new value
//! or the next refresh will fail with `invalid_grant`.
//!
//! Endpoints, client IDs, and payload shapes are mirrored from the upstream
//! CLIs (Claude Code, Codex). Changing them silently breaks operator pairings.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Outcome of a refresh call. `expires_at_ms` is wall-clock millis at which
/// the new access token expires.
#[derive(Debug, Clone)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Claude Code / Anthropic-OAuth refresh.
///
/// Issues `POST /v1/oauth/token` against `console.anthropic.com` with a
/// form-encoded `grant_type=refresh_token` payload. The `client_id` is the
/// well-known Claude Code CLI id; both that and the endpoint must match what
/// the upstream CLI uses or the refresh server returns `invalid_client`.
pub async fn refresh_anthropic(refresh_token: &str) -> Result<RefreshedTokens> {
    refresh_anthropic_against(
        refresh_token,
        "https://console.anthropic.com/v1/oauth/token",
    )
    .await
}

/// Same as [`refresh_anthropic`] but with a caller-supplied endpoint URL —
/// the test path drives this against a `wiremock` server.
pub async fn refresh_anthropic_against(
    refresh_token: &str,
    endpoint: &str,
) -> Result<RefreshedTokens> {
    if refresh_token.is_empty() {
        return Err(anyhow!("refresh_token is empty"));
    }
    let client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build http client")?;
    let resp = client
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("anthropic oauth refresh: send")?;
    let status = resp.status();
    let body = resp.text().await.context("anthropic oauth refresh: body")?;
    if !status.is_success() {
        return Err(anyhow!(
            "anthropic oauth refresh: HTTP {status}: {}",
            truncate(&body, 256)
        ));
    }
    let parsed: OAuthTokenResponse = serde_json::from_str(&body)
        .with_context(|| format!("parse anthropic refresh body: {}", truncate(&body, 256)))?;
    if parsed.access_token.is_empty() {
        return Err(anyhow!("anthropic refresh response missing access_token"));
    }
    let next_refresh = parsed
        .refresh_token
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| refresh_token.to_string());
    let expires_in = parsed.expires_in.unwrap_or(3600);
    Ok(RefreshedTokens {
        access_token: parsed.access_token,
        refresh_token: next_refresh,
        expires_at_ms: now_ms() + (expires_in as i64) * 1000,
    })
}

/// ChatGPT Codex refresh. Endpoint is `auth.openai.com/oauth/token`; the
/// client_id is the public Codex CLI id.
pub async fn refresh_codex(refresh_token: &str) -> Result<RefreshedTokens> {
    refresh_codex_against(refresh_token, "https://auth.openai.com/oauth/token").await
}

pub async fn refresh_codex_against(refresh_token: &str, endpoint: &str) -> Result<RefreshedTokens> {
    if refresh_token.is_empty() {
        return Err(anyhow!("refresh_token is empty"));
    }
    let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build http client")?;
    let resp = client
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("codex oauth refresh: send")?;
    let status = resp.status();
    let body = resp.text().await.context("codex oauth refresh: body")?;
    if !status.is_success() {
        return Err(anyhow!(
            "codex oauth refresh: HTTP {status}: {}",
            truncate(&body, 256)
        ));
    }
    let parsed: OAuthTokenResponse = serde_json::from_str(&body)
        .with_context(|| format!("parse codex refresh body: {}", truncate(&body, 256)))?;
    if parsed.access_token.is_empty() {
        return Err(anyhow!("codex refresh response missing access_token"));
    }
    let next_refresh = parsed
        .refresh_token
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| refresh_token.to_string());
    // Codex doesn't always return `expires_in`; ChatGPT access tokens are
    // ~7 days, so default to that when missing.
    let expires_in = parsed.expires_in.unwrap_or(7 * 24 * 3600);
    Ok(RefreshedTokens {
        access_token: parsed.access_token,
        refresh_token: next_refresh,
        expires_at_ms: now_ms() + (expires_in as i64) * 1000,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    crate::util::truncate::truncate_with_ellipsis(s, max, "…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn anthropic_refresh_happy_path_rotates_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_string_contains("refresh_token=old-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/v1/oauth/token", server.uri());
        let r = refresh_anthropic_against("old-refresh", &url)
            .await
            .unwrap();
        assert_eq!(r.access_token, "new-access");
        assert_eq!(r.refresh_token, "new-refresh");
        assert!(r.expires_at_ms > now_ms());
    }

    #[tokio::test]
    async fn anthropic_refresh_preserves_old_refresh_when_response_omits_it() {
        // Some refresh responses return just access_token. Falling back to
        // the input refresh_token keeps the credential usable until the
        // provider does rotate it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"access_token":"new-access","expires_in":3600}"#),
            )
            .mount(&server)
            .await;
        let url = format!("{}/v1/oauth/token", server.uri());
        let r = refresh_anthropic_against("preserved-refresh", &url)
            .await
            .unwrap();
        assert_eq!(r.refresh_token, "preserved-refresh");
    }

    #[tokio::test]
    async fn anthropic_refresh_failure_surfaces_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string(
                r#"{"error":"invalid_grant","error_description":"refresh token consumed"}"#,
            ))
            .mount(&server)
            .await;
        let url = format!("{}/v1/oauth/token", server.uri());
        let err = refresh_anthropic_against("stale", &url).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "must surface status: {msg}");
        assert!(
            msg.contains("invalid_grant"),
            "must surface body for debugging: {msg}"
        );
    }

    #[tokio::test]
    async fn codex_refresh_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains(
                "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"cx-new","refresh_token":"cx-new-r","expires_in":604800}"#,
            ))
            .mount(&server)
            .await;
        let url = format!("{}/oauth/token", server.uri());
        let r = refresh_codex_against("cx-old", &url).await.unwrap();
        assert_eq!(r.access_token, "cx-new");
        assert_eq!(r.refresh_token, "cx-new-r");
    }

    #[tokio::test]
    async fn empty_refresh_token_fails_fast() {
        assert!(refresh_anthropic_against("", "http://unused")
            .await
            .is_err());
        assert!(refresh_codex_against("", "http://unused").await.is_err());
    }
}
