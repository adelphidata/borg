use anyhow::{Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, Request};
use sha2::{Digest, Sha256};
use url::Url;

use crate::llm::{FailoverReason, LlmError};

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "bedrock";

#[derive(Clone, Debug)]
pub(crate) struct AwsCredentials {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
}

pub(crate) fn url_for_model(region: &str, model: &str, stream: bool) -> String {
    let suffix = if stream {
        "converse-stream"
    } else {
        "converse"
    };
    format!("https://bedrock-runtime.{region}.amazonaws.com/model/{model}/{suffix}")
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

    let credentials = resolve_env_credentials()
        .or_else(|| resolve_profile_credentials(&profile))
        .context("AWS credential chain did not resolve credentials from env or ~/.aws profiles")?;

    Ok((region, credentials))
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
    })
}

fn resolve_profile_credentials(profile: &str) -> Option<AwsCredentials> {
    let data = read_aws_profiles("credentials")?;
    let section = data.get(profile)?;
    Some(AwsCredentials {
        access_key_id: section.get("aws_access_key_id")?.clone(),
        secret_access_key: section.get("aws_secret_access_key")?.clone(),
        session_token: section.get("aws_session_token").cloned(),
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
        let unary = url_for_model(
            "us-east-1",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            false,
        );
        assert_eq!(
            unary,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-7-sonnet-20250219-v1:0/converse"
        );
        let stream = url_for_model(
            "eu-west-2",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            true,
        );
        assert_eq!(
            stream,
            "https://bedrock-runtime.eu-west-2.amazonaws.com/model/anthropic.claude-3-5-haiku-20241022-v1:0/converse-stream"
        );
    }

    #[test]
    fn signed_request_emits_sigv4_headers() {
        let creds = AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: Some("session-token-xyz".to_string()),
        };
        let client = Client::new();
        let url = url_for_model(
            "us-east-1",
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
}
