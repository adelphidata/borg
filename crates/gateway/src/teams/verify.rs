use anyhow::{bail, Result};
use axum::http::HeaderMap;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a Microsoft Teams webhook request signature using HMAC-SHA256.
///
/// Teams signs requests with an HMAC-SHA256 over the raw body bytes, using a
/// base64-decoded shared secret. The signature is sent in the `Authorization`
/// header as `HMAC <base64-signature>`.
pub fn verify_teams_signature(headers: &HeaderMap, body: &[u8], secret: &str) -> Result<()> {
    let auth_header =
        crate::verify_common::required_header(headers, "authorization", "Authorization")?;

    let signature_b64 = auth_header
        .strip_prefix("HMAC ")
        .ok_or_else(|| anyhow::anyhow!("Authorization header must start with 'HMAC '"))?;

    let expected_sig = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .map_err(|e| anyhow::anyhow!("Invalid base64 in Authorization header: {e}"))?;

    let secret_bytes = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .map_err(|e| anyhow::anyhow!("Invalid base64 secret: {e}"))?;

    let mut mac = HmacSha256::new_from_slice(&secret_bytes)
        .map_err(|e| anyhow::anyhow!("HMAC key error: {e}"))?;
    mac.update(body);
    let computed = mac.finalize().into_bytes();

    if !crate::crypto::constant_time_eq(&computed, &expected_sig) {
        bail!("Teams signature verification failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn make_secret() -> String {
        // A known base64-encoded secret for testing
        base64::engine::general_purpose::STANDARD.encode(b"test-secret-key-32bytes-long!!")
    }

    fn compute_signature(secret_b64: &str, body: &[u8]) -> String {
        let secret_bytes = base64::engine::general_purpose::STANDARD
            .decode(secret_b64)
            .unwrap();
        let mut mac = HmacSha256::new_from_slice(&secret_bytes).expect("HMAC accepts any key size");
        mac.update(body);
        let result = mac.finalize().into_bytes();
        base64::engine::general_purpose::STANDARD.encode(result)
    }

    fn make_headers(signature_b64: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = format!("HMAC {signature_b64}");
        headers.insert("authorization", HeaderValue::from_str(&value).unwrap());
        headers
    }

    #[test]
    fn valid_signature_passes() {
        let secret = make_secret();
        let body = b"hello teams";
        let sig = compute_signature(&secret, body);
        let headers = make_headers(&sig);

        assert!(verify_teams_signature(&headers, body, &secret).is_ok());
    }

    #[test]
    fn wrong_secret_fails() {
        let secret = make_secret();
        let wrong_secret =
            base64::engine::general_purpose::STANDARD.encode(b"wrong-key!!!!!!!!!!!!!!!!!!!!!!!");
        let body = b"hello teams";
        let sig = compute_signature(&secret, body);
        let headers = make_headers(&sig);

        let result = verify_teams_signature(&headers, body, &wrong_secret);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("verification failed"));
    }

    #[test]
    fn tampered_body_fails() {
        let secret = make_secret();
        let body = b"hello teams";
        let sig = compute_signature(&secret, body);
        let headers = make_headers(&sig);

        let result = verify_teams_signature(&headers, b"tampered body", &secret);
        assert!(result.is_err());
    }

    #[test]
    fn missing_authorization_header_fails() {
        let headers = HeaderMap::new();
        let result = verify_teams_signature(&headers, b"body", &make_secret());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing Authorization"));
    }

    #[test]
    fn wrong_auth_prefix_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token123"));
        let result = verify_teams_signature(&headers, b"body", &make_secret());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HMAC"));
    }

    #[test]
    fn invalid_base64_signature_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("HMAC !!!not-base64!!!"),
        );
        let result = verify_teams_signature(&headers, b"body", &make_secret());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base64"));
    }
}
