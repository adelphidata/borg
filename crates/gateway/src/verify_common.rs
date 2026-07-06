//! Shared helpers for webhook signature / token verification.
//!
//! Each native channel reimplements the same header-extraction boilerplate
//! before doing channel-specific crypto. This module centralizes that
//! extraction so the error messages stay uniform.

use anyhow::{bail, Result};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Canonical HTTP header names used for webhook signature / token verification.
///
/// Centralized so production extraction paths and test fixtures cannot drift —
/// a typo in either side would silently break signature verification.
pub mod headers {
    pub const SLACK_TIMESTAMP: &str = "x-slack-request-timestamp";
    pub const SLACK_SIGNATURE: &str = "x-slack-signature";
    pub const DISCORD_SIGNATURE: &str = "x-signature-ed25519";
    pub const DISCORD_TIMESTAMP: &str = "x-signature-timestamp";
    pub const TELEGRAM_SECRET: &str = "x-telegram-bot-api-secret-token";
    pub const AUTHORIZATION: &str = "authorization";
}

/// Gate inbound webhook processing on a configured verification secret.
///
/// Fail CLOSED. A native channel activates on its bot/API token, but inbound
/// request authentication uses a SEPARATE signing secret (Slack signing secret,
/// Discord public key, Telegram webhook secret). If that secret is absent — or
/// blank — signature verification was being silently skipped, and because the
/// sender identity in the payload is attacker-controlled JSON, an unauthenticated
/// POST could drive the agent as the owner. When the credential is missing we
/// must refuse the request rather than trust it. Mirrors the fail-closed posture
/// Google Chat and Twilio already enforce.
///
/// `env_var` names the credential in the returned error so operators can fix the
/// misconfiguration.
pub fn require_verification_secret(
    channel: &str,
    secret: Option<&str>,
    env_var: &str,
) -> Result<()> {
    if secret.map(str::trim).is_none_or(str::is_empty) {
        bail!(
            "{channel} inbound webhook refused: no verification secret configured. \
             Set {env_var} to authenticate inbound requests (failing closed)."
        );
    }
    Ok(())
}

/// Compute HMAC-SHA256 over `data` keyed by `key` and constant-time compare
/// against the expected raw signature bytes. Used by Slack (hex-encoded
/// expected) and Teams (base64-encoded expected); each caller decodes its
/// transport encoding before invoking this.
pub fn hmac_sha256_verify(
    key: &[u8],
    data: &[u8],
    expected: &[u8],
    label: &'static str,
) -> Result<()> {
    // Reject empty signing secrets up-front. HMAC over an empty key is
    // mathematically well-defined, so a request signed with `""` would
    // verify against a misconfigured (empty) secret — a silent auth bypass.
    if key.is_empty() {
        bail!("{label} signing secret is empty — refusing to verify");
    }
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|e| anyhow::anyhow!("HMAC key error: {e}"))?;
    mac.update(data);
    let computed = mac.finalize().into_bytes();
    if !crate::crypto::constant_time_eq(&computed, expected) {
        bail!("{label} signature verification failed");
    }
    Ok(())
}

/// Fetch a required header and decode it as UTF-8.
///
/// Returns an error if the header is absent, not ASCII, or contains invalid
/// UTF-8 bytes. The error uses `display_name` for the user-facing message so
/// callers can use lowercased keys (which `HeaderMap` requires) while keeping
/// the canonical camel-case name in logs.
pub fn required_header<'a>(
    headers: &'a HeaderMap,
    key: &str,
    display_name: &str,
) -> Result<&'a str> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("Missing {display_name} header"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn returns_value_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("v1"));
        assert_eq!(required_header(&headers, "x-test", "X-Test").unwrap(), "v1");
    }

    #[test]
    fn errors_when_missing() {
        let headers = HeaderMap::new();
        let err = required_header(&headers, "x-test", "X-Test").unwrap_err();
        assert!(err.to_string().contains("Missing X-Test header"));
    }

    #[test]
    fn hmac_rejects_empty_key() {
        // Regression: empty secret used to verify against any signature.
        let err = hmac_sha256_verify(b"", b"data", b"sig", "Test").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn errors_when_not_utf8() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap());
        assert!(required_header(&headers, "x-test", "X-Test").is_err());
    }

    #[test]
    fn require_verification_secret_fails_closed_when_missing_or_blank() {
        // The whole point: a channel that is live but has no verification
        // secret must refuse inbound traffic, not silently accept it.
        for absent in [None, Some(""), Some("   "), Some("\t\n")] {
            let err = require_verification_secret("slack", absent, "SLACK_SIGNING_SECRET")
                .expect_err("missing/blank secret must fail closed");
            assert!(
                err.to_string().contains("SLACK_SIGNING_SECRET"),
                "error should name the missing credential: {err}"
            );
        }
    }

    #[test]
    fn require_verification_secret_allows_configured_secret() {
        assert!(
            require_verification_secret("slack", Some("s3cr3t"), "SLACK_SIGNING_SECRET").is_ok(),
            "a configured secret must be accepted"
        );
    }
}
