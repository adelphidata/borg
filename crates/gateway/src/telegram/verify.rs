use anyhow::{bail, Result};
use axum::http::HeaderMap;

use crate::verify_common::headers as hdr;

use super::types::Update;

/// Verify the `X-Telegram-Bot-Api-Secret-Token` header using constant-time comparison.
pub fn verify_secret_token(headers: &HeaderMap, secret: &str) -> bool {
    let header_val = match headers.get(hdr::TELEGRAM_SECRET) {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => return false,
        },
        None => return false,
    };

    crate::crypto::constant_time_eq(header_val.as_bytes(), secret.as_bytes())
}

/// Parse and validate a Telegram update from the request body.
pub fn validate_update(body: &str) -> Result<Update> {
    let update: Update = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Invalid Telegram update JSON: {e}"))?;

    if update.update_id == 0 {
        bail!("Invalid update: update_id is zero");
    }

    Ok(update)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn make_headers(secret: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(s) = secret {
            headers.insert(hdr::TELEGRAM_SECRET, HeaderValue::from_str(s).unwrap());
        }
        headers
    }

    #[test]
    fn correct_secret_verifies() {
        let headers = make_headers(Some("my-secret-123"));
        assert!(verify_secret_token(&headers, "my-secret-123"));
    }

    #[test]
    fn wrong_secret_fails() {
        let headers = make_headers(Some("wrong-secret"));
        assert!(!verify_secret_token(&headers, "my-secret-123"));
    }

    #[test]
    fn missing_header_fails() {
        let headers = make_headers(None);
        assert!(!verify_secret_token(&headers, "my-secret-123"));
    }

    #[test]
    fn empty_secret_matches_empty_header() {
        let headers = make_headers(Some(""));
        assert!(verify_secret_token(&headers, ""));
    }

    #[test]
    fn valid_update_body() {
        let body = r#"{
            "update_id": 12345,
            "message": {
                "message_id": 1,
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "text": "hello"
            }
        }"#;

        let update = validate_update(body).unwrap();
        assert_eq!(update.update_id, 12345);
    }

    #[test]
    fn invalid_json_body() {
        assert!(validate_update("not json").is_err());
    }

    #[test]
    fn missing_update_id_field() {
        assert!(validate_update(r#"{ "message": {} }"#).is_err());
    }
}
