use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, Request};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use url::Url;

use crate::llm::{FailoverReason, LlmError};

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "bedrock";

/// Default ECS/EKS container-credential metadata endpoint (link-local; the
/// relative URI from `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` hangs off this).
const ECS_CREDENTIALS_DEFAULT_BASE: &str = "http://169.254.170.2";
/// Default EC2 Instance Metadata Service (IMDSv2) base.
const IMDS_DEFAULT_BASE: &str = "http://169.254.169.254";
/// TTL requested for the IMDSv2 session token (seconds, AWS max 21600).
const IMDS_TOKEN_TTL_SECS: u32 = 21600;
/// Per-request timeout for credential-metadata fetches. Short so hosts that are
/// not on EC2/ECS fail fast to the next chain link instead of hanging.
const CREDENTIAL_FETCH_TIMEOUT_SECS: u64 = 2;
/// Refresh cached temporary credentials this many seconds before their real
/// expiry so an in-flight signing can't race the deadline with a dead key.
const CREDENTIAL_CACHE_SKEW_SECS: i64 = 60;

#[derive(Clone, Debug)]
pub(crate) struct AwsCredentials {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
    /// Expiry for temporary credentials (ECS/IMDS). `None` for long-lived
    /// static credentials (env / `~/.aws`), which never need refreshing.
    pub(crate) expires_at: Option<DateTime<Utc>>,
}

impl AwsCredentials {
    /// Whether these credentials are safe to reuse from cache — static
    /// credentials always are; temporary ones only until the skew window
    /// before their expiry.
    fn is_fresh(&self) -> bool {
        match self.expires_at {
            None => true,
            Some(expiry) => {
                Utc::now() + chrono::Duration::seconds(CREDENTIAL_CACHE_SKEW_SECS) < expiry
            }
        }
    }
}

/// Compose the full Converse / ConverseStream URL for a model against a
/// resolved base endpoint (scheme + host, no trailing slash needed).
pub(crate) fn url_for_model(base: &str, model: &str, stream: bool) -> String {
    let suffix = if stream {
        "converse-stream"
    } else {
        "converse"
    };
    let base = base.trim_end_matches('/');
    format!("{base}/model/{model}/{suffix}")
}

/// Rebuild a Bedrock request URL by swapping only the origin (scheme, host,
/// port) of the `endpoint()` hint with the freshly-resolved `base`, preserving
/// the request path.
///
/// The path already carries any operator base-path from the override (the hint
/// was built with `url_for_model(base_with_path, …)`), so rebuilding as
/// `base + path` would double the base-path prefix (e.g. `/bedrock/bedrock/…`)
/// and 404 every PrivateLink / gateway request. Swapping the origin also lets
/// the async credential chain correct a placeholder region in the derived host
/// before signing.
pub(crate) fn rebuild_request_url(request_url: &str, base: &str) -> Result<String> {
    let mut parsed = Url::parse(request_url).context("parse Bedrock URL hint")?;
    let base = Url::parse(base).context("parse resolved Bedrock endpoint base")?;
    parsed
        .set_scheme(base.scheme())
        .map_err(|_| anyhow::anyhow!("invalid scheme in resolved Bedrock endpoint"))?;
    parsed
        .set_host(base.host_str())
        .context("apply resolved Bedrock host")?;
    parsed
        .set_port(base.port())
        .map_err(|_| anyhow::anyhow!("invalid port in resolved Bedrock endpoint"))?;
    Ok(parsed.to_string())
}

/// Normalize an operator-supplied Bedrock endpoint override into a
/// `scheme://host[/base-path]` URL with no trailing slash. A bare host (no
/// scheme) defaults to `https://`. Returns `None` for blank input.
pub(crate) fn normalize_endpoint_override(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("://") {
        Some(trimmed.to_string())
    } else {
        Some(format!("https://{trimmed}"))
    }
}

/// AWS regions for which AWS publishes a `bedrock-runtime-fips` endpoint.
/// Toggling FIPS for any other region derives a host that does not resolve, so
/// we warn rather than fail silently at connect time.
fn region_has_fips_endpoint(region: &str) -> bool {
    matches!(
        region,
        "us-east-1"
            | "us-east-2"
            | "us-west-1"
            | "us-west-2"
            | "us-gov-east-1"
            | "us-gov-west-1"
            | "ca-central-1"
    )
}

/// Whether a host is a cloud instance-metadata endpoint. Bedrock requests are
/// SigV4-signed with live AWS credentials (including the session token), so an
/// endpoint override pointing at IMDS / ECS-credential / other-cloud metadata
/// services would let a poisoned setting exfiltrate those credentials. This
/// blocks the entire link-local range (covers 169.254.169.254 IMDS and
/// 169.254.170.2 ECS credentials) plus the known metadata FQDNs, while leaving
/// RFC1918 addresses alone so PrivateLink / VPC-endpoint hosts still work.
pub(crate) fn is_cloud_metadata_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                let seg = v6.segments();
                // fe80::/10 link-local, or the AWS IMDS IPv6 address.
                (seg[0] & 0xffc0) == 0xfe80 || seg == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
            }
        };
    }
    matches!(
        h.to_ascii_lowercase().as_str(),
        "metadata.google.internal" | "metadata.goog" | "metadata"
    )
}

/// Validate and normalize an operator-supplied endpoint override into a base URL.
///
/// Rejects non-`https` schemes (Bedrock requests carry SigV4 credentials and
/// must never travel in cleartext) and cloud instance-metadata hosts (an
/// attacker-controlled override would otherwise sign requests with live AWS
/// credentials straight to IMDS / ECS metadata).
fn validate_endpoint_override(raw: &str, source: &str) -> Result<String> {
    let normalized = normalize_endpoint_override(raw)
        .ok_or_else(|| anyhow::anyhow!("{source} endpoint override is blank"))?;
    let url = Url::parse(&normalized)
        .with_context(|| format!("{source} endpoint override is not a valid URL: {normalized}"))?;
    if url.scheme() != "https" {
        anyhow::bail!(
            "{source} endpoint override must use https (got {}://); Bedrock requests carry SigV4 \
             credentials and must not travel in cleartext",
            url.scheme()
        );
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{source} endpoint override has no host: {normalized}"))?;
    if is_cloud_metadata_host(host) {
        anyhow::bail!(
            "{source} endpoint override host {host} is a cloud instance-metadata endpoint; \
             refusing to sign Bedrock requests with live AWS credentials to it"
        );
    }
    Ok(normalized)
}

/// Validate an operator endpoint override (https-only, non cloud-metadata
/// host) and return the normalized base URL. Exposed for the air-gap gate,
/// which must apply the same guard before treating an override as a private
/// PrivateLink path.
pub(crate) fn validated_endpoint_override(raw: &str) -> Result<String> {
    validate_endpoint_override(raw, "llm.bedrock_endpoint_url")
}

/// Resolve the base Bedrock runtime endpoint (scheme + host, no path).
///
/// Precedence: explicit `endpoint_override` (from `llm.bedrock_endpoint_url`) →
/// the `AWS_ENDPOINT_URL_BEDROCK` environment variable → the derived
/// `bedrock-runtime[-fips].{region}.amazonaws.com` host.
///
/// GovCloud regions (`us-gov-west-1`, `us-gov-east-1`) live in the same
/// `.amazonaws.com` DNS suffix as the commercial partition, so the derived host
/// is correct for them without special-casing. `use_fips` selects the
/// FIPS 140-2 validated `-fips` host required by many US-government compliance
/// regimes. An explicit endpoint override always wins over the FIPS toggle —
/// airgapped / PrivateLink (VPC interface endpoint) deployments point Borg at a
/// private hostname that already terminates at the operator's chosen endpoint.
///
/// Overrides are validated: https-only and never a cloud-metadata host, so a
/// poisoned setting cannot redirect SigV4-signed, credential-bearing requests.
pub(crate) fn resolve_endpoint_base(
    region: &str,
    endpoint_override: Option<&str>,
    use_fips: bool,
) -> Result<String> {
    if let Some(raw) = endpoint_override.filter(|s| !s.trim().is_empty()) {
        return validate_endpoint_override(raw, "llm.bedrock_endpoint_url");
    }
    if let Ok(env) = std::env::var("AWS_ENDPOINT_URL_BEDROCK") {
        if !env.trim().is_empty() {
            return validate_endpoint_override(&env, "AWS_ENDPOINT_URL_BEDROCK");
        }
    }
    if use_fips && !region_has_fips_endpoint(region) {
        tracing::warn!(
            region,
            "llm.bedrock_use_fips is set but AWS publishes no Bedrock FIPS endpoint for this \
             region; the derived bedrock-runtime-fips host will not resolve"
        );
    }
    let host = if use_fips {
        format!("bedrock-runtime-fips.{region}.amazonaws.com")
    } else {
        format!("bedrock-runtime.{region}.amazonaws.com")
    };
    Ok(format!("https://{host}"))
}

pub(crate) fn resolve_region(explicit_region: Option<&str>) -> Option<String> {
    let profile = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("AWS_DEFAULT_PROFILE")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "default".to_string());

    explicit_region
        .filter(|region| !region.trim().is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::env::var("AWS_DEFAULT_REGION")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| resolve_profile_region(&profile))
}

pub(crate) async fn load_region_and_credentials(
    client: &Client,
    explicit_region: Option<&str>,
) -> Result<(String, AwsCredentials)> {
    let profile = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("AWS_DEFAULT_PROFILE")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "default".to_string());

    let region = resolve_region(explicit_region)
        .ok_or_else(|| anyhow::anyhow!("AWS region chain did not resolve a region"))?;

    // Static sources first — no network, no caching, highest precedence
    // (mirrors the AWS SDK default provider chain).
    if let Some(credentials) =
        resolve_env_credentials().or_else(|| resolve_profile_credentials(&profile))
    {
        return Ok((region, credentials));
    }

    // Temporary sources (ECS container metadata, then EC2 IMDS). These return
    // short-lived credentials with an expiry, so a fresh cached copy is reused
    // across requests until the skew window before it lapses.
    if let Some(credentials) = cached_dynamic_credentials() {
        return Ok((region, credentials));
    }
    if let Some(credentials) = resolve_container_credentials(client).await? {
        return Ok((region, store_dynamic_credentials(credentials)));
    }
    if let Some(credentials) = resolve_imds_credentials(client).await? {
        return Ok((region, store_dynamic_credentials(credentials)));
    }

    bail!(
        "AWS credential chain did not resolve credentials from env, ~/.aws profiles, \
         ECS container metadata, or EC2 IMDS"
    )
}

/// Cached temporary credentials from the ECS/IMDS providers. Keyed by nothing —
/// a process signs for a single AWS identity, so one slot suffices; the entry is
/// discarded once [`AwsCredentials::is_fresh`] turns false.
static DYNAMIC_CREDENTIAL_CACHE: OnceLock<Mutex<Option<AwsCredentials>>> = OnceLock::new();

fn dynamic_credential_cache() -> &'static Mutex<Option<AwsCredentials>> {
    DYNAMIC_CREDENTIAL_CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_dynamic_credentials() -> Option<AwsCredentials> {
    let guard = dynamic_credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clone().filter(AwsCredentials::is_fresh)
}

fn store_dynamic_credentials(credentials: AwsCredentials) -> AwsCredentials {
    let mut guard = dynamic_credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(credentials.clone());
    credentials
}

#[cfg(test)]
pub(crate) fn clear_dynamic_credential_cache_for_tests() {
    let mut guard = dynamic_credential_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// AWS container/IMDS credential JSON payload. Both the ECS credential relative
/// URI and the IMDS `iam/security-credentials/<role>` document share this shape
/// (extra fields like `Code`/`RoleArn` are ignored).
#[derive(Deserialize)]
struct AwsJsonCredentials {
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "Token", default)]
    token: Option<String>,
    #[serde(rename = "Expiration", default)]
    expiration: Option<String>,
}

async fn parse_aws_json_credentials(
    resp: reqwest::Response,
    label: &str,
) -> Result<AwsCredentials> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("read {label} response body"))?;
    if !status.is_success() {
        bail!(
            "{label} request returned HTTP {status}: {}",
            truncate(&body)
        );
    }
    let parsed: AwsJsonCredentials =
        serde_json::from_str(&body).with_context(|| format!("parse {label} response"))?;
    if parsed.access_key_id.trim().is_empty() || parsed.secret_access_key.trim().is_empty() {
        bail!("{label} response missing AccessKeyId/SecretAccessKey");
    }
    let expires_at = parsed
        .expiration
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            DateTime::parse_from_rfc3339(s.trim())
                .map(|dt| dt.with_timezone(&Utc))
                .with_context(|| format!("parse {label} Expiration timestamp {s:?}"))
        })
        .transpose()?;
    Ok(AwsCredentials {
        access_key_id: parsed.access_key_id,
        secret_access_key: parsed.secret_access_key,
        session_token: parsed.token.filter(|t| !t.trim().is_empty()),
        expires_at,
    })
}

/// Resolve ECS/EKS container credentials when `AWS_CONTAINER_CREDENTIALS_*` is
/// configured. Returns `Ok(None)` when the container provider is not configured
/// (so the chain falls through to IMDS); a configured-but-failing endpoint is a
/// hard error, matching the AWS SDK.
async fn resolve_container_credentials(client: &Client) -> Result<Option<AwsCredentials>> {
    let url = if let Some(full) = env_nonempty("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        validate_container_full_uri(&full)?;
        full
    } else if let Some(relative) = env_nonempty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        let base = env_nonempty("BORG_AWS_ECS_CREDENTIALS_BASE")
            .unwrap_or_else(|| ECS_CREDENTIALS_DEFAULT_BASE.to_string());
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            relative.trim_start_matches('/')
        )
    } else {
        return Ok(None);
    };

    let mut request = client
        .get(&url)
        .timeout(Duration::from_secs(CREDENTIAL_FETCH_TIMEOUT_SECS));
    if let Some(auth) = container_auth_token()? {
        request = request.header(reqwest::header::AUTHORIZATION, auth);
    }
    let resp = request
        .send()
        .await
        .with_context(|| format!("ECS container credential request to {url} failed"))?;
    parse_aws_json_credentials(resp, "ECS container credentials")
        .await
        .map(Some)
}

/// Validate an `AWS_CONTAINER_CREDENTIALS_FULL_URI`: https to any host, or http
/// only to loopback/link-local (ECS agent, EKS pod identity). Refuses cleartext
/// to arbitrary hosts so a poisoned env var cannot exfiltrate the auth token.
fn validate_container_full_uri(raw: &str) -> Result<()> {
    let url = Url::parse(raw)
        .with_context(|| format!("parse AWS_CONTAINER_CREDENTIALS_FULL_URI {raw:?}"))?;
    let host = url
        .host_str()
        .context("AWS_CONTAINER_CREDENTIALS_FULL_URI missing host")?;
    let loopback_or_link_local = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback() || is_link_local_ip(&ip))
        .unwrap_or(false)
        || host.eq_ignore_ascii_case("localhost");
    match url.scheme() {
        "https" => {
            if !loopback_or_link_local {
                // Standard AWS SDK allows https anywhere, but a poisoned env var
                // could still exfiltrate the container auth token over TLS to an
                // attacker host — surface it rather than sending silently.
                tracing::warn!(
                    host,
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI targets a non-loopback host; \
                     the container credential auth token will be sent to it"
                );
            }
            Ok(())
        }
        "http" => {
            if loopback_or_link_local {
                Ok(())
            } else {
                bail!(
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI over http must target loopback or \
                     link-local (got host {host}); refusing to send the credential token in \
                     cleartext to an arbitrary host"
                )
            }
        }
        other => bail!("AWS_CONTAINER_CREDENTIALS_FULL_URI must be http or https, got {other}://"),
    }
}

fn is_link_local_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

fn container_auth_token() -> Result<Option<String>> {
    if let Some(file) = env_nonempty("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        let token = std::fs::read_to_string(&file)
            .with_context(|| format!("read AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE {file}"))?;
        return Ok(Some(token.trim().to_string()));
    }
    Ok(env_nonempty("AWS_CONTAINER_AUTHORIZATION_TOKEN"))
}

/// Resolve EC2 instance-role credentials via IMDSv2 (token-guarded). Returns
/// `Ok(None)` when no session token can be obtained (host is not an EC2 instance
/// or IMDS is disabled) so the chain ends with a clear "no credentials" error
/// rather than a confusing metadata failure.
async fn resolve_imds_credentials(client: &Client) -> Result<Option<AwsCredentials>> {
    let base =
        env_nonempty("BORG_AWS_IMDS_ENDPOINT").unwrap_or_else(|| IMDS_DEFAULT_BASE.to_string());
    let base = base.trim_end_matches('/');

    let token = match client
        .put(format!("{base}/latest/api/token"))
        .header(
            "x-aws-ec2-metadata-token-ttl-seconds",
            IMDS_TOKEN_TTL_SECS.to_string(),
        )
        .timeout(Duration::from_secs(CREDENTIAL_FETCH_TIMEOUT_SECS))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .text()
            .await
            .context("read IMDSv2 session token")?
            .trim()
            .to_string(),
        Ok(resp) => {
            tracing::warn!(
                status = %resp.status(),
                "IMDSv2 token endpoint returned non-success; skipping IMDS credentials"
            );
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "IMDSv2 token request failed; host is likely not an EC2 instance"
            );
            return Ok(None);
        }
    };
    if token.is_empty() {
        return Ok(None);
    }

    let role_resp = client
        .get(format!("{base}/latest/meta-data/iam/security-credentials/"))
        .header("x-aws-ec2-metadata-token", &token)
        .timeout(Duration::from_secs(CREDENTIAL_FETCH_TIMEOUT_SECS))
        .send()
        .await
        .context("IMDS instance-role lookup failed")?;
    let role_status = role_resp.status();
    let role_body = role_resp
        .text()
        .await
        .context("read IMDS instance-role list")?;
    if !role_status.is_success() {
        bail!(
            "IMDS instance-role lookup returned HTTP {role_status}: {}",
            truncate(&role_body)
        );
    }
    let role = role_body
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .context("IMDS returned no IAM role attached to this instance")?;

    let resp = client
        .get(format!(
            "{base}/latest/meta-data/iam/security-credentials/{role}"
        ))
        .header("x-aws-ec2-metadata-token", &token)
        .timeout(Duration::from_secs(CREDENTIAL_FETCH_TIMEOUT_SECS))
        .send()
        .await
        .context("IMDS credential request failed")?;
    parse_aws_json_credentials(resp, "EC2 IMDS credentials")
        .await
        .map(Some)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn truncate(s: &str) -> String {
    const MAX: usize = 512;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &s[..end])
}

pub(crate) fn signed_request(
    client: &Client,
    url: &str,
    body: &[u8],
    region: &str,
    credentials: &AwsCredentials,
    mut headers: HeaderMap,
) -> Result<Request> {
    let timestamp = Utc::now();
    let amz_datetime = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
    let date = timestamp.format("%Y%m%d").to_string();
    let payload_hash = hex_sha256(body);

    let parsed_url = Url::parse(url).with_context(|| format!("parse Bedrock URL {url}"))?;
    let host = parsed_url
        .host_str()
        .context("Bedrock URL missing host")?
        .to_string();

    insert_header(&mut headers, "host", &host)?;
    insert_header(&mut headers, "x-amz-date", &amz_datetime)?;
    insert_header(&mut headers, "x-amz-content-sha256", &payload_hash)?;
    if let Some(token) = credentials.session_token.as_deref() {
        insert_header(&mut headers, "x-amz-security-token", token)?;
    }

    let canonical_uri = if parsed_url.path().is_empty() {
        "/"
    } else {
        parsed_url.path()
    };
    let canonical_query = canonical_query_string(&parsed_url);
    let (canonical_headers, signed_headers) = canonical_headers(&headers)?;
    let canonical_request = format!(
        "POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let credential_scope = format!("{date}/{region}/{SERVICE}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_datetime}\n{credential_scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );
    let signing_key = signing_key(&credentials.secret_access_key, &date, region, SERVICE)?;
    let signature = hex_hmac(&signing_key, &string_to_sign)?;
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    insert_header(&mut headers, "authorization", &authorization)?;

    client
        .request(Method::POST, url)
        .headers(headers)
        .body(body.to_vec())
        .build()
        .context("build signed Bedrock request")
}

pub(crate) fn try_take_eventstream_payload(
    buffer: &mut Vec<u8>,
) -> std::result::Result<Option<Vec<u8>>, LlmError> {
    if buffer.len() < 12 {
        return Ok(None);
    }

    let total_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let headers_len = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;

    if total_len < 16 {
        return Err(LlmError::Fatal {
            source: anyhow::anyhow!("Bedrock eventstream frame too short: {total_len} bytes"),
            reason: FailoverReason::Format,
        });
    }
    if buffer.len() < total_len {
        return Ok(None);
    }

    let payload_start = 12 + headers_len;
    let payload_end = total_len.saturating_sub(4);
    if payload_start > payload_end || payload_end > total_len {
        return Err(LlmError::Fatal {
            source: anyhow::anyhow!("Malformed Bedrock eventstream frame"),
            reason: FailoverReason::Format,
        });
    }

    let payload = buffer[payload_start..payload_end].to_vec();
    buffer.drain(..total_len);
    Ok(Some(payload))
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_bytes(name.as_bytes()).context("invalid Bedrock header name")?,
        HeaderValue::from_str(value).context("invalid Bedrock header value")?,
    );
    Ok(())
}

fn canonical_query_string(url: &Url) -> String {
    let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", aws_percent_encode(&k), aws_percent_encode(&v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_headers(headers: &HeaderMap) -> Result<(String, String)> {
    let mut entries: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            Ok((
                name.as_str().to_ascii_lowercase(),
                value
                    .to_str()
                    .context("Bedrock signed header is not valid UTF-8")?
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical = entries
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed = entries
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    Ok((canonical, signed))
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Result<Vec<u8>> {
    let k_date = hmac_bytes(format!("AWS4{secret}").as_bytes(), date)?;
    let k_region = hmac_bytes(&k_date, region)?;
    let k_service = hmac_bytes(&k_region, service)?;
    hmac_bytes(&k_service, "aws4_request")
}

fn hmac_bytes(key: &[u8], data: &str) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key).context("init HMAC")?;
    mac.update(data.as_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_hmac(key: &[u8], data: &str) -> Result<String> {
    Ok(hex_encode(&hmac_bytes(key, data)?))
}

fn hex_sha256(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

fn aws_percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn resolve_env_credentials() -> Option<AwsCredentials> {
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let session_token = std::env::var("AWS_SESSION_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    Some(AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token,
        expires_at: None,
    })
}

fn resolve_profile_credentials(profile: &str) -> Option<AwsCredentials> {
    let data = read_aws_profiles("credentials")?;
    let section = data.get(profile)?;
    Some(AwsCredentials {
        access_key_id: section.get("aws_access_key_id")?.clone(),
        secret_access_key: section.get("aws_secret_access_key")?.clone(),
        session_token: section.get("aws_session_token").cloned(),
        expires_at: None,
    })
}

fn resolve_profile_region(profile: &str) -> Option<String> {
    let data = read_aws_profiles("config")?;
    data.get(&format!("profile {profile}"))
        .or_else(|| data.get(profile))
        .and_then(|section| section.get("region").cloned())
}

fn read_aws_profiles(
    file_name: &str,
) -> Option<std::collections::HashMap<String, std::collections::HashMap<String, String>>> {
    let path = dirs::home_dir()?.join(".aws").join(file_name);
    let contents = std::fs::read_to_string(path).ok()?;
    let mut sections = std::collections::HashMap::new();
    let mut current_section: Option<String> = None;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name.trim().to_string();
            current_section = Some(name.clone());
            sections
                .entry(name)
                .or_insert_with(std::collections::HashMap::new);
            continue;
        }
        let Some(section) = current_section.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        sections
            .entry(section.clone())
            .or_insert_with(std::collections::HashMap::new)
            .insert(key.trim().to_string(), value.trim().to_string());
    }

    Some(sections)
}

/// Process-wide lock for tests that mutate `HOME` / `AWS_*` env vars. Cross-
/// crate tests reach in here so every env-touching Bedrock test serializes
/// against one mutex rather than racing the runtime.
#[cfg(test)]
pub(crate) fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.old.as_deref() {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn aws_percent_encoding_matches_sigv4_rules() {
        assert_eq!(aws_percent_encode("a b"), "a%20b");
        assert_eq!(aws_percent_encode(":/"), "%3A%2F");
        assert_eq!(aws_percent_encode("~ok"), "~ok");
    }

    #[test]
    fn eventstream_payload_extraction_handles_complete_frame() {
        let payload = br#"{"type":"contentBlockDelta"}"#;
        let headers = [];
        let total_len = 16 + payload.len() + headers.len();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&0_u32.to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0_u32.to_be_bytes());

        let got = try_take_eventstream_payload(&mut frame)
            .expect("frame parses")
            .expect("payload present");
        assert_eq!(got, payload);
        assert!(frame.is_empty(), "frame bytes must be drained");
    }

    #[test]
    fn env_credentials_win_when_present() {
        let _lock = super::env_test_lock();
        let _access = EnvGuard::set("AWS_ACCESS_KEY_ID", "AKIA_TEST");
        let _secret = EnvGuard::set("AWS_SECRET_ACCESS_KEY", "secret");
        let _token = EnvGuard::set("AWS_SESSION_TOKEN", "token");
        let creds = resolve_env_credentials().expect("env creds");
        assert_eq!(creds.access_key_id, "AKIA_TEST");
        assert_eq!(creds.secret_access_key, "secret");
        assert_eq!(creds.session_token.as_deref(), Some("token"));
    }

    #[test]
    fn url_for_model_distinguishes_stream_and_unary() {
        let base = "https://bedrock-runtime.us-east-1.amazonaws.com";
        let unary = url_for_model(base, "anthropic.claude-3-7-sonnet-20250219-v1:0", false);
        assert_eq!(
            unary,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-7-sonnet-20250219-v1:0/converse"
        );
        let stream = url_for_model(
            "https://bedrock-runtime.eu-west-2.amazonaws.com",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            true,
        );
        assert_eq!(
            stream,
            "https://bedrock-runtime.eu-west-2.amazonaws.com/model/anthropic.claude-3-5-haiku-20241022-v1:0/converse-stream"
        );
    }

    #[test]
    fn url_for_model_trims_trailing_slash_on_base() {
        // A PrivateLink override may arrive with a trailing slash; the joined
        // path must not double up separators.
        let url = url_for_model(
            "https://vpce-abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com/",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            false,
        );
        assert_eq!(
            url,
            "https://vpce-abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"
        );
    }

    #[test]
    fn rebuild_request_url_swaps_region_host_without_base_path() {
        // Derived (no base-path) case: the async chain corrects the region in
        // the host while preserving the model path.
        let hint = url_for_model(
            "https://bedrock-runtime.region-unresolved.amazonaws.com",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            false,
        );
        let out = rebuild_request_url(&hint, "https://bedrock-runtime.us-gov-west-1.amazonaws.com")
            .unwrap();
        assert_eq!(
            out,
            "https://bedrock-runtime.us-gov-west-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"
        );
    }

    #[test]
    fn rebuild_request_url_preserves_operator_base_path_without_doubling() {
        // Regression: a gateway/PrivateLink base-path override must not be
        // duplicated into `/bedrock/bedrock/...` (which 404s every request).
        let base = "https://ai-gateway.corp/bedrock";
        let hint = url_for_model(base, "anthropic.claude-3-5-sonnet-20241022-v2:0", true);
        // The hint already carries the base-path once.
        assert_eq!(
            hint,
            "https://ai-gateway.corp/bedrock/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse-stream"
        );
        let out = rebuild_request_url(&hint, base).unwrap();
        assert_eq!(
            out,
            "https://ai-gateway.corp/bedrock/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse-stream",
            "base-path must appear exactly once"
        );
        assert!(
            !out.contains("/bedrock/bedrock/"),
            "base-path was doubled: {out}"
        );
    }

    #[test]
    fn rebuild_request_url_preserves_custom_port() {
        let hint = url_for_model(
            "https://10.20.30.40:8443",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            false,
        );
        let out = rebuild_request_url(&hint, "https://10.20.30.40:8443").unwrap();
        assert_eq!(
            out,
            "https://10.20.30.40:8443/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"
        );
    }

    #[test]
    fn normalize_endpoint_override_adds_scheme_and_trims() {
        assert_eq!(
            normalize_endpoint_override("bedrock.internal.example"),
            Some("https://bedrock.internal.example".to_string())
        );
        assert_eq!(
            normalize_endpoint_override("http://10.0.0.5:8443/"),
            Some("http://10.0.0.5:8443".to_string())
        );
        assert_eq!(
            normalize_endpoint_override("https://host/base/"),
            Some("https://host/base".to_string())
        );
        assert_eq!(normalize_endpoint_override("   "), None);
        assert_eq!(normalize_endpoint_override(""), None);
    }

    #[test]
    fn resolve_endpoint_base_derives_standard_host() {
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert_eq!(
            resolve_endpoint_base("us-east-1", None, false).unwrap(),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn resolve_endpoint_base_govcloud_region_uses_amazonaws_com() {
        // GovCloud regions share the commercial .amazonaws.com suffix; no
        // partition special-casing needed.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert_eq!(
            resolve_endpoint_base("us-gov-west-1", None, false).unwrap(),
            "https://bedrock-runtime.us-gov-west-1.amazonaws.com"
        );
    }

    #[test]
    fn resolve_endpoint_base_fips_selects_fips_host() {
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert_eq!(
            resolve_endpoint_base("us-gov-west-1", None, true).unwrap(),
            "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com"
        );
    }

    #[test]
    fn resolve_endpoint_base_explicit_override_beats_fips_and_env() {
        // Airgapped / PrivateLink: an explicit endpoint always wins, even with
        // FIPS requested and a differing env var set.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::set("AWS_ENDPOINT_URL_BEDROCK", "https://env-endpoint.example");
        assert_eq!(
            resolve_endpoint_base(
                "us-gov-west-1",
                Some("vpce-0abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com"),
                true,
            )
            .unwrap(),
            "https://vpce-0abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com"
        );
    }

    #[test]
    fn resolve_endpoint_base_env_var_used_when_no_explicit_override() {
        let _lock = super::env_test_lock();
        let _env = EnvGuard::set("AWS_ENDPOINT_URL_BEDROCK", "https://env-endpoint.internal/");
        assert_eq!(
            resolve_endpoint_base("us-east-1", None, false).unwrap(),
            "https://env-endpoint.internal"
        );
    }

    #[test]
    fn resolve_endpoint_base_rejects_http_override() {
        // http:// would send SigV4 credentials in cleartext; must be refused.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        let err = resolve_endpoint_base("us-east-1", Some("http://10.0.0.5:8443"), false)
            .expect_err("http override must be rejected");
        assert!(
            err.to_string().contains("https"),
            "error should mention https requirement: {err}"
        );
    }

    #[test]
    fn resolve_endpoint_base_rejects_imds_metadata_override() {
        // The classic SSRF-to-credential-exfil target must be refused even
        // though it is https.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        let err = resolve_endpoint_base("us-east-1", Some("https://169.254.169.254/latest"), false)
            .expect_err("IMDS override must be rejected");
        assert!(
            err.to_string().contains("metadata"),
            "error should mention metadata endpoint: {err}"
        );
    }

    #[test]
    fn resolve_endpoint_base_rejects_ecs_credentials_metadata_override() {
        // 169.254.170.2 is the ECS task-credentials endpoint — also link-local.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert!(
            resolve_endpoint_base("us-east-1", Some("https://169.254.170.2/creds"), false).is_err(),
            "ECS credential metadata host must be rejected"
        );
    }

    #[test]
    fn resolve_endpoint_base_rejects_gcp_metadata_fqdn_override() {
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert!(
            resolve_endpoint_base("us-east-1", Some("https://metadata.google.internal"), false)
                .is_err(),
            "GCP metadata FQDN must be rejected"
        );
    }

    #[test]
    fn resolve_endpoint_base_rejects_metadata_host_from_env_var() {
        // The guard must apply to the env-var source too, not just the setting.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::set("AWS_ENDPOINT_URL_BEDROCK", "https://169.254.169.254");
        assert!(
            resolve_endpoint_base("us-east-1", None, false).is_err(),
            "metadata host via AWS_ENDPOINT_URL_BEDROCK must be rejected"
        );
    }

    #[test]
    fn resolve_endpoint_base_allows_rfc1918_privatelink_host() {
        // PrivateLink / VPC-endpoint hosts resolve to RFC1918 addresses and
        // MUST remain reachable — the metadata guard is link-local only.
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert_eq!(
            resolve_endpoint_base("us-gov-west-1", Some("https://10.20.30.40:8443"), false)
                .unwrap(),
            "https://10.20.30.40:8443"
        );
    }

    #[test]
    fn resolve_endpoint_base_fips_supported_region_ok() {
        let _lock = super::env_test_lock();
        let _env = EnvGuard::remove("AWS_ENDPOINT_URL_BEDROCK");
        assert!(region_has_fips_endpoint("us-gov-west-1"));
        assert!(region_has_fips_endpoint("us-east-1"));
        assert!(!region_has_fips_endpoint("eu-west-2"));
    }

    #[test]
    fn signed_request_emits_sigv4_headers() {
        let creds = AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: Some("session-token-xyz".to_string()),
            expires_at: None,
        };
        let client = Client::new();
        let url = url_for_model(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            false,
        );
        let body = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let req = signed_request(&client, &url, body, "us-east-1", &creds, headers)
            .expect("signs request");
        let h = req.headers();

        // Required AWS headers populated.
        assert!(h.contains_key("x-amz-date"), "missing x-amz-date");
        assert!(
            h.contains_key("x-amz-content-sha256"),
            "missing x-amz-content-sha256"
        );
        assert_eq!(
            h.get("x-amz-security-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "session-token-xyz",
            "session token must be passed through when present"
        );

        // Authorization header has the right algorithm, scope, and lists the
        // signed headers in alphabetical order — the canonical-request
        // contract `signing_key` depends on.
        let auth = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .expect("authorization header set");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"),
            "wrong algorithm/credential prefix: {auth}"
        );
        assert!(
            auth.contains("/us-east-1/bedrock/aws4_request"),
            "wrong credential scope: {auth}"
        );
        assert!(
            auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"),
            "signed headers list must be alphabetized and include all signed headers: {auth}"
        );
        // Extract the signature and assert it's a 64-char lowercase hex digest
        // (HMAC-SHA256 hex output). That's the AWS SigV4 contract; rubber-
        // stamping a longer length would silently accept malformed signatures.
        let signature = auth
            .rsplit_once(", Signature=")
            .map(|(_, sig)| sig)
            .expect("Signature= component present");
        assert_eq!(
            signature.len(),
            64,
            "signature must be 64 hex chars: {auth}"
        );
        assert!(
            signature
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "signature must be lowercase hex: {signature}"
        );
    }

    #[test]
    fn resolve_region_reads_shared_aws_profile_config() {
        let _guard = super::env_test_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let aws_dir = temp.path().join(".aws");
        fs::create_dir_all(&aws_dir).expect("create .aws");
        fs::write(
            aws_dir.join("config"),
            "[profile team]\nregion = eu-central-1\n",
        )
        .expect("write aws config");

        let _home = EnvGuard::set("HOME", temp.path().to_str().expect("home path"));
        let _profile = EnvGuard::set("AWS_PROFILE", "team");
        let _default_profile = EnvGuard::remove("AWS_DEFAULT_PROFILE");
        let _aws_region = EnvGuard::remove("AWS_REGION");
        let _default_region = EnvGuard::remove("AWS_DEFAULT_REGION");

        let region = resolve_region(None).expect("region from shared config");
        assert_eq!(region, "eu-central-1");
    }

    fn creds_json(expiration: &str) -> serde_json::Value {
        serde_json::json!({
            "AccessKeyId": "ASIADYNAMIC",
            "SecretAccessKey": "dynamic-secret",
            "Token": "dynamic-session-token",
            "Expiration": expiration,
            "Code": "Success",
        })
    }

    #[test]
    fn is_fresh_reflects_expiry_with_skew() {
        let mut creds = AwsCredentials {
            access_key_id: "a".into(),
            secret_access_key: "b".into(),
            session_token: None,
            expires_at: None,
        };
        // Static creds (no expiry) are always reusable.
        assert!(creds.is_fresh());
        // Expiry comfortably in the future → fresh.
        creds.expires_at = Some(Utc::now() + chrono::Duration::seconds(3600));
        assert!(creds.is_fresh());
        // Already past → stale.
        creds.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(!creds.is_fresh());
        // Inside the refresh-skew window → treated as stale so we refresh early.
        creds.expires_at =
            Some(Utc::now() + chrono::Duration::seconds(CREDENTIAL_CACHE_SKEW_SECS - 5));
        assert!(!creds.is_fresh());
    }

    #[test]
    fn dynamic_credential_cache_round_trips_and_evicts_stale() {
        let _lock = super::env_test_lock();
        clear_dynamic_credential_cache_for_tests();
        assert!(cached_dynamic_credentials().is_none());

        let fresh = AwsCredentials {
            access_key_id: "ASIAFRESH".into(),
            secret_access_key: "s".into(),
            session_token: Some("t".into()),
            expires_at: Some(Utc::now() + chrono::Duration::seconds(3600)),
        };
        store_dynamic_credentials(fresh);
        assert_eq!(
            cached_dynamic_credentials()
                .expect("fresh cached")
                .access_key_id,
            "ASIAFRESH"
        );

        let stale = AwsCredentials {
            access_key_id: "ASIASTALE".into(),
            secret_access_key: "s".into(),
            session_token: None,
            expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
        };
        store_dynamic_credentials(stale);
        assert!(
            cached_dynamic_credentials().is_none(),
            "stale credentials must not be served from cache"
        );
        clear_dynamic_credential_cache_for_tests();
    }

    #[test]
    fn full_uri_over_http_rejected_for_public_host() {
        let err = validate_container_full_uri("http://169.254.169.254/creds")
            .err()
            .map(|e| e.to_string());
        assert!(
            validate_container_full_uri("http://example.com/creds").is_err(),
            "cleartext to arbitrary host must be refused"
        );
        // Link-local (ECS agent) and loopback over http are allowed.
        assert!(validate_container_full_uri("http://127.0.0.1:9000/creds").is_ok());
        assert!(
            err.is_none(),
            "link-local http endpoint should be accepted: {err:?}"
        );
        // https to any host is fine.
        assert!(validate_container_full_uri("https://creds.example.com/x").is_ok());
        // A non-http(s) scheme is refused.
        assert!(validate_container_full_uri("file:///etc/passwd").is_err());
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn container_credentials_from_relative_uri_parse_and_expire() {
        let _lock = super::env_test_lock();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/credentials/abc"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(creds_json("2999-01-01T00:00:00Z")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let _full = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_FULL_URI");
        let _rel = EnvGuard::set(
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "/v2/credentials/abc",
        );
        let _base = EnvGuard::set("BORG_AWS_ECS_CREDENTIALS_BASE", &server.uri());
        let _authf = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
        let _auth = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN");

        let creds = resolve_container_credentials(&Client::new())
            .await
            .expect("container request ok")
            .expect("credentials present");
        assert_eq!(creds.access_key_id, "ASIADYNAMIC");
        assert_eq!(creds.secret_access_key, "dynamic-secret");
        assert_eq!(
            creds.session_token.as_deref(),
            Some("dynamic-session-token")
        );
        assert_eq!(
            creds.expires_at,
            Some(
                DateTime::parse_from_rfc3339("2999-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn container_credentials_full_uri_sends_auth_token() {
        let _lock = super::env_test_lock();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/creds"))
            .and(header("authorization", "Bearer ecs-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(creds_json("2999-01-01T00:00:00Z")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let _rel = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        let _full = EnvGuard::set(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            &format!("{}/creds", server.uri()),
        );
        let _authf = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
        let _auth = EnvGuard::set("AWS_CONTAINER_AUTHORIZATION_TOKEN", "Bearer ecs-token");

        let creds = resolve_container_credentials(&Client::new())
            .await
            .expect("container request ok")
            .expect("credentials present");
        assert_eq!(creds.access_key_id, "ASIADYNAMIC");
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn container_credentials_none_when_unconfigured() {
        let _lock = super::env_test_lock();
        let _full = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_FULL_URI");
        let _rel = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        let got = resolve_container_credentials(&Client::new())
            .await
            .expect("no error when unconfigured");
        assert!(got.is_none());
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn imds_v2_happy_path_signs_token_and_reads_role() {
        let _lock = super::env_test_lock();
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/latest/api/token"))
            .and(header("x-aws-ec2-metadata-token-ttl-seconds", "21600"))
            .respond_with(ResponseTemplate::new(200).set_body_string("imds-token-xyz"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest/meta-data/iam/security-credentials/"))
            .and(header("x-aws-ec2-metadata-token", "imds-token-xyz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("borg-instance-role\n"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/latest/meta-data/iam/security-credentials/borg-instance-role",
            ))
            .and(header("x-aws-ec2-metadata-token", "imds-token-xyz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(creds_json("2999-01-01T00:00:00Z")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let _endpoint = EnvGuard::set("BORG_AWS_IMDS_ENDPOINT", &server.uri());
        let creds = resolve_imds_credentials(&Client::new())
            .await
            .expect("imds request ok")
            .expect("credentials present");
        assert_eq!(creds.access_key_id, "ASIADYNAMIC");
        assert_eq!(
            creds.session_token.as_deref(),
            Some("dynamic-session-token")
        );
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn imds_returns_none_when_token_forbidden() {
        let _lock = super::env_test_lock();
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/latest/api/token"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let _endpoint = EnvGuard::set("BORG_AWS_IMDS_ENDPOINT", &server.uri());
        let got = resolve_imds_credentials(&Client::new())
            .await
            .expect("no hard error when IMDS unavailable");
        assert!(
            got.is_none(),
            "a host without IMDSv2 must fall through, not error"
        );
    }

    #[tokio::test]
    // Holds `env_test_lock` across `.await` on purpose: the resolver reads
    // AWS_* env vars, so the vars must stay put for the whole request.
    #[allow(clippy::await_holding_lock)]
    async fn load_credentials_prefers_env_over_dynamic_metadata() {
        let _lock = super::env_test_lock();
        clear_dynamic_credential_cache_for_tests();
        let _region = EnvGuard::set("AWS_REGION", "us-gov-west-1");
        let _access = EnvGuard::set("AWS_ACCESS_KEY_ID", "AKIASTATIC");
        let _secret = EnvGuard::set("AWS_SECRET_ACCESS_KEY", "static-secret");
        let _sess = EnvGuard::remove("AWS_SESSION_TOKEN");
        // A metadata endpoint is configured but must never be consulted.
        let _full = EnvGuard::set(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "http://127.0.0.1:1/creds",
        );

        let (region, creds) = load_region_and_credentials(&Client::new(), None)
            .await
            .expect("static env credentials resolve");
        assert_eq!(region, "us-gov-west-1");
        assert_eq!(creds.access_key_id, "AKIASTATIC");
        assert_eq!(creds.expires_at, None);
        clear_dynamic_credential_cache_for_tests();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn container_endpoint_failure_is_a_hard_error() {
        let _lock = super::env_test_lock();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/creds"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let _rel = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        let _authf = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
        let _auth = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN");
        let _full = EnvGuard::set(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            &format!("{}/creds", server.uri()),
        );
        // A configured-but-failing container endpoint must surface as an error,
        // not silently fall through to IMDS (matches the AWS SDK).
        let err = resolve_container_credentials(&Client::new())
            .await
            .expect_err("5xx from the container endpoint must be a hard error");
        assert!(
            err.to_string().contains("HTTP 500"),
            "error should carry the HTTP status: {err}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn load_credentials_walks_to_container_and_caches_result() {
        let _lock = super::env_test_lock();
        clear_dynamic_credential_cache_for_tests();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/creds"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(creds_json("2999-01-01T00:00:00Z")),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Neutralize static sources: no env keys, and an empty HOME so the
        // shared-profile file lookup finds nothing.
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard::set("HOME", temp.path().to_str().expect("home path"));
        let _access = EnvGuard::remove("AWS_ACCESS_KEY_ID");
        let _secret = EnvGuard::remove("AWS_SECRET_ACCESS_KEY");
        let _sess = EnvGuard::remove("AWS_SESSION_TOKEN");
        let _profile = EnvGuard::remove("AWS_PROFILE");
        let _region = EnvGuard::set("AWS_REGION", "us-east-1");
        let _rel = EnvGuard::remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        let _authf = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
        let _auth = EnvGuard::remove("AWS_CONTAINER_AUTHORIZATION_TOKEN");
        let _full = EnvGuard::set(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            &format!("{}/creds", server.uri()),
        );

        let (region, creds) = load_region_and_credentials(&Client::new(), None)
            .await
            .expect("chain resolves via container metadata");
        assert_eq!(region, "us-east-1");
        assert_eq!(creds.access_key_id, "ASIADYNAMIC");
        // The dynamic result must now be cached (the mock is mounted `.expect(1)`,
        // so a second network hit would fail verification on drop).
        let cached = cached_dynamic_credentials().expect("dynamic creds cached");
        assert_eq!(cached.access_key_id, "ASIADYNAMIC");
        clear_dynamic_credential_cache_for_tests();
    }
}
