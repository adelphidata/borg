use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

const AZURE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
const AZURE_IMDS_RESOURCE: &str = "https://cognitiveservices.azure.com/";
const GCP_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_GCP_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
// Expire cached tokens 60s early so an in-flight request can't race the
// real expiry and hit the provider with a just-dead token.
const CACHE_SKEW_SECS: u64 = 60;

#[derive(Clone, Debug)]
pub(crate) struct CloudToken {
    pub(crate) access_token: String,
    expires_at: SystemTime,
}

impl CloudToken {
    fn new(access_token: String, expires_in: u64) -> Self {
        let ttl = expires_in.saturating_sub(CACHE_SKEW_SECS).max(1);
        Self {
            access_token,
            expires_at: SystemTime::now() + Duration::from_secs(ttl),
        }
    }

    fn is_valid(&self) -> bool {
        SystemTime::now() < self.expires_at
    }
}

static TOKEN_CACHE: OnceLock<Mutex<HashMap<String, CloudToken>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, CloudToken>> {
    TOKEN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(key: &str) -> Option<CloudToken> {
    let guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.get(key).filter(|token| token.is_valid()).cloned()
}

fn store(key: &str, token: CloudToken) -> CloudToken {
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert(key.to_string(), token.clone());
    token
}

#[cfg(test)]
pub(crate) fn clear_cache_for_tests() {
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clear();
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub(crate) async fn azure_openai_token(client: &Client) -> Result<CloudToken> {
    let cache_key = format!(
        "azure:{}:{}:{}:{}",
        std::env::var("AZURE_TENANT_ID").unwrap_or_default(),
        std::env::var("AZURE_CLIENT_ID").unwrap_or_default(),
        std::env::var("BORG_AZURE_TOKEN_URL").unwrap_or_default(),
        std::env::var("BORG_AZURE_IMDS_ENDPOINT").unwrap_or_default()
    );
    if let Some(token) = cached(&cache_key) {
        return Ok(token);
    }

    let token = if std::env::var("AZURE_TENANT_ID").is_ok()
        && std::env::var("AZURE_CLIENT_ID").is_ok()
        && std::env::var("AZURE_CLIENT_SECRET").is_ok()
    {
        azure_client_credentials_token(client).await?
    } else {
        azure_managed_identity_token(client).await?
    };
    Ok(store(&cache_key, token))
}

async fn azure_client_credentials_token(client: &Client) -> Result<CloudToken> {
    let tenant = std::env::var("AZURE_TENANT_ID").context("AZURE_TENANT_ID is required")?;
    let client_id = std::env::var("AZURE_CLIENT_ID").context("AZURE_CLIENT_ID is required")?;
    let client_secret =
        std::env::var("AZURE_CLIENT_SECRET").context("AZURE_CLIENT_SECRET is required")?;
    let url = std::env::var("BORG_AZURE_TOKEN_URL").unwrap_or_else(|_| {
        format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token")
    });

    let resp = client
        .post(&url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("scope", AZURE_SCOPE),
        ])
        .send()
        .await
        .context("Azure OpenAI Entra token request failed")?;
    parse_token_response(resp, "Azure OpenAI Entra token").await
}

async fn azure_managed_identity_token(client: &Client) -> Result<CloudToken> {
    let endpoint = std::env::var("BORG_AZURE_IMDS_ENDPOINT")
        .unwrap_or_else(|_| "http://169.254.169.254/metadata/identity/oauth2/token".to_string());
    let mut url = reqwest::Url::parse(&endpoint).context("parse Azure IMDS endpoint")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("api-version", "2018-02-01");
        pairs.append_pair("resource", AZURE_IMDS_RESOURCE);
        if let Ok(client_id) = std::env::var("AZURE_CLIENT_ID") {
            if !client_id.trim().is_empty() {
                pairs.append_pair("client_id", &client_id);
            }
        }
    }
    let resp = client
        .get(url)
        .header("Metadata", "true")
        .send()
        .await
        .context("Azure managed identity token request failed")?;
    parse_token_response(resp, "Azure managed identity token").await
}

pub(crate) async fn gcp_access_token(client: &Client) -> Result<CloudToken> {
    let cache_key = format!(
        "gcp:{}:{}:{}",
        std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_default(),
        std::env::var("BORG_GCP_TOKEN_URL").unwrap_or_default(),
        std::env::var("BORG_GCP_METADATA_TOKEN_URL").unwrap_or_default()
    );
    if let Some(token) = cached(&cache_key) {
        return Ok(token);
    }

    let token = if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        gcp_service_account_token(client, &path).await?
    } else {
        gcp_metadata_token(client).await?
    };
    Ok(store(&cache_key, token))
}

#[derive(Deserialize)]
struct ServiceAccountJson {
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
}

async fn gcp_service_account_token(client: &Client, path: &str) -> Result<CloudToken> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read GOOGLE_APPLICATION_CREDENTIALS file {path}"))?;
    let sa: ServiceAccountJson =
        serde_json::from_str(&raw).context("parse service-account JSON")?;
    let token_url = std::env::var("BORG_GCP_TOKEN_URL")
        .ok()
        .or(sa.token_uri)
        .unwrap_or_else(|| DEFAULT_GCP_TOKEN_URL.to_string());
    let assertion = service_account_jwt(&sa.client_email, &sa.private_key, &token_url)?;

    let resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .context("GCP service-account token request failed")?;
    parse_token_response(resp, "GCP service-account token").await
}

async fn gcp_metadata_token(client: &Client) -> Result<CloudToken> {
    let endpoint = std::env::var("BORG_GCP_METADATA_TOKEN_URL").unwrap_or_else(|_| {
        "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token"
            .to_string()
    });
    let mut url = reqwest::Url::parse(&endpoint).context("parse GCP metadata token endpoint")?;
    url.query_pairs_mut().append_pair("scopes", GCP_SCOPE);
    let resp = client
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .context("GCP metadata token request failed")?;
    parse_token_response(resp, "GCP metadata token").await
}

async fn parse_token_response(resp: reqwest::Response, label: &str) -> Result<CloudToken> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .context("read cloud token response body")?;
    if !status.is_success() {
        bail!(
            "{label} request returned HTTP {status}: {}",
            truncate(&body)
        );
    }
    let parsed: TokenResponse =
        serde_json::from_str(&body).with_context(|| format!("parse {label} response"))?;
    if parsed.access_token.trim().is_empty() {
        bail!("{label} response missing access_token");
    }
    Ok(CloudToken::new(
        parsed.access_token,
        parsed.expires_in.unwrap_or(3600),
    ))
}

fn service_account_jwt(
    client_email: &str,
    private_key_pem: &str,
    token_url: &str,
) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX_EPOCH")?
        .as_secs();
    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": client_email,
        "scope": GCP_SCOPE,
        "aud": token_url,
        "iat": now,
        "exp": now + 3600,
    });
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let signing_input = format!(
        "{}.{}",
        enc.encode(serde_json::to_vec(&header)?),
        enc.encode(serde_json::to_vec(&claims)?)
    );
    let key_der = first_pkcs8_key(private_key_pem)?;
    let key_pair = ring::signature::RsaKeyPair::from_pkcs8(&key_der)
        .map_err(|_| anyhow!("parse service-account RSA private key"))?;
    let rng = ring::rand::SystemRandom::new();
    let mut signature = vec![0; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &rng,
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| anyhow!("sign service-account JWT"))?;
    Ok(format!("{}.{}", signing_input, enc.encode(signature)))
}

fn first_pkcs8_key(private_key_pem: &str) -> Result<Vec<u8>> {
    let mut reader = Cursor::new(private_key_pem.as_bytes());
    let key = rustls_pemfile::pkcs8_private_keys(&mut reader)
        .next()
        .transpose()
        .context("read PKCS#8 private key PEM")?
        .context("service-account JSON private_key has no PKCS#8 key")?;
    Ok(key.secret_pkcs8_der().to_vec())
}

/// Bounds GCP auth diagnostic text, independent of the provider response-body
/// cap so retuning one does not silently retune the other.
const DIAGNOSTIC_MAX_BYTES: usize = 512;

fn truncate(s: &str) -> String {
    crate::util::truncate::truncate_bytes(s, DIAGNOSTIC_MAX_BYTES, "...[truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn azure_client_credentials_posts_expected_scope() {
        clear_cache_for_tests();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tenant/oauth2/v2.0/token"))
            .and(body_string_contains("grant_type=client_credentials"))
            .and(body_string_contains(
                "scope=https%3A%2F%2Fcognitiveservices.azure.com%2F.default",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "azure-token",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        std::env::set_var("AZURE_TENANT_ID", "tenant");
        std::env::set_var("AZURE_CLIENT_ID", "client");
        std::env::set_var("AZURE_CLIENT_SECRET", "secret");
        std::env::set_var(
            "BORG_AZURE_TOKEN_URL",
            format!("{}/tenant/oauth2/v2.0/token", server.uri()),
        );

        let token = azure_openai_token(&Client::new()).await.unwrap();
        assert_eq!(token.access_token, "azure-token");

        std::env::remove_var("AZURE_TENANT_ID");
        std::env::remove_var("AZURE_CLIENT_ID");
        std::env::remove_var("AZURE_CLIENT_SECRET");
        std::env::remove_var("BORG_AZURE_TOKEN_URL");
    }
}
