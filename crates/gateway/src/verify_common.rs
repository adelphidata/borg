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
    fn errors_when_not_utf8() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap());
        assert!(required_header(&headers, "x-test", "X-Test").is_err());
    }
}
