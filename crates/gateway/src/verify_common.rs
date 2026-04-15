//! Shared helpers for webhook signature / token verification.
//!
//! Each native channel reimplements the same header-extraction boilerplate
//! before doing channel-specific crypto. This module centralizes that
//! extraction so the error messages stay uniform.

use anyhow::Result;
use axum::http::HeaderMap;

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
