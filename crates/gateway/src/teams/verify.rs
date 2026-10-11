use anyhow::Result;
use axum::http::HeaderMap;
use base64::Engine;

use crate::verify_common::headers as hdr;

/// Verify a Microsoft Teams webhook request signature using HMAC-SHA256.
///
/// Teams signs requests with an HMAC-SHA256 over the raw body bytes, using a
/// base64-decoded shared secret. The signature is sent in the `Authorization`
/// header as `HMAC <base64-signature>`.
pub fn verify_teams_signature(headers: &HeaderMap, body: &[u8], secret: &str) -> Result<()> {
    let auth_header =
        crate::verify_common::required_header(headers, hdr::AUTHORIZATION, "Authorization")?;

    let signature_b64 = auth_header
        .strip_prefix("HMAC ")
        .ok_or_else(|| anyhow::anyhow!("Authorization header must start with 'HMAC '"))?;

    let expected_sig = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .map_err(|e| anyhow::anyhow!("Invalid base64 in Authorization header: {e}"))?;

    let secret_bytes = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .map_err(|e| anyhow::anyhow!("Invalid base64 secret: {e}"))?;

    crate::verify_common::hmac_sha256_verify(&secret_bytes, body, &expected_sig, "Teams")
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
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret_bytes = base64::engine::general_purpose::STANDARD
            .decode(secret_b64)
            .unwrap();
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&secret_bytes).expect("HMAC accepts any key size");
        mac.update(body);
        let result = mac.finalize().into_bytes();
        base64::engine::general_purpose::STANDARD.encode(result)
    }

    fn make_headers(signature_b64: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = format!("HMAC {signature_b64}");
        headers.insert(hdr::AUTHORIZATION, HeaderValue::from_str(&value).unwrap());
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

    /// A secret that base64-decodes to all-NUL (`AA==`) or all-whitespace
    /// (`ICAg` is three spaces) must fail closed even when the attacker signs
    /// with that very key: HMAC zero-pads short keys, so `[0x00]` is the empty
    /// key, and a blank key is a placeholder an operator never meant to deploy.
    #[test]
    fn degenerate_decoded_secret_fails_even_with_matching_signature() {
        let body = b"hello teams";
        for secret in ["AA==", "AAAAAAAA", "ICAg", "CQo="] {
            let sig = compute_signature(secret, body);
            let headers = make_headers(&sig);
            let err = verify_teams_signature(&headers, body, secret)
                .expect_err(&format!("secret {secret}: degenerate key must be refused"));
            assert!(
                err.to_string().contains("empty or blank"),
                "secret {secret}: must fail at the key gate, not the comparison: {err}"
            );
        }
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
        headers.insert(
            hdr::AUTHORIZATION,
            HeaderValue::from_static("Bearer token123"),
        );
        let result = verify_teams_signature(&headers, b"body", &make_secret());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HMAC"));
    }

    #[test]
    fn invalid_base64_signature_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            hdr::AUTHORIZATION,
            HeaderValue::from_static("HMAC !!!not-base64!!!"),
        );
        let result = verify_teams_signature(&headers, b"body", &make_secret());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base64"));
    }
}
