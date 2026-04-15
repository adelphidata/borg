use anyhow::Result;
use axum::http::HeaderMap;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// Verify a Twilio webhook request signature.
///
/// Twilio signs webhooks with HMAC-SHA1:
///   1. Start with the full webhook URL
///   2. Sort POST params alphabetically by key, append key+value
///   3. HMAC-SHA1 with auth token, Base64-encode
///   4. Compare against X-Twilio-Signature header
pub fn verify_twilio_signature(
    headers: &HeaderMap,
    webhook_url: &str,
    body: &str,
    auth_token: &str,
) -> Result<()> {
    let signature =
        crate::verify_common::required_header(headers, "x-twilio-signature", "X-Twilio-Signature")?;

    let expected = compute_signature(webhook_url, body, auth_token)?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|e| anyhow::anyhow!("Invalid base64 in X-Twilio-Signature: {e}"))?;
    let expected_bytes = base64::engine::general_purpose::STANDARD
        .decode(&expected)
        .map_err(|e| anyhow::anyhow!("Internal base64 error: {e}"))?;

    if crate::crypto::constant_time_eq(&sig_bytes, &expected_bytes) {
        Ok(())
    } else {
        anyhow::bail!("Twilio signature verification failed")
    }
}

/// Compute the expected Twilio signature for a request.
/// Returns an error if HMAC key construction fails or body parsing fails.
pub fn compute_signature(url: &str, body: &str, auth_token: &str) -> Result<String> {
    let mut data_string = url.to_string();

    // Parse form-urlencoded params and sort alphabetically.
    // Fail closed on malformed bodies — unwrap_or_default would let an
    // attacker submit an empty param vector, altering the HMAC input.
    let mut params: Vec<(String, String)> = serde_urlencoded::from_str(body)
        .map_err(|e| anyhow::anyhow!("Failed to parse Twilio webhook body: {e}"))?;
    params.sort_by(|a, b| a.0.cmp(&b.0));

    for (key, value) in &params {
        data_string.push_str(key);
        data_string.push_str(value);
    }

    let mut mac = HmacSha1::new_from_slice(auth_token.as_bytes())
        .map_err(|e| anyhow::anyhow!("HMAC key error: {e}"))?;
    mac.update(data_string.as_bytes());
    let result = mac.finalize();

    Ok(base64::engine::general_purpose::STANDARD.encode(result.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn compute_signature_deterministic() {
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let body = "CallSid=CA1234567890ABCDE&Caller=%2B14158675310&Digits=1234&From=%2B14158675310&To=%2B18005551212";
        let auth_token = "12345";

        let sig = compute_signature(url, body, auth_token).unwrap();
        assert!(!sig.is_empty());
        assert_eq!(sig, compute_signature(url, body, auth_token).unwrap());
    }

    #[test]
    fn verify_valid_signature() {
        let url = "https://example.com/webhook/twilio";
        let body = "Body=Hello&From=%2B14155551234&To=%2B14155555678&MessageSid=SM123";
        let auth_token = "test-auth-token";

        let expected_sig = compute_signature(url, body, auth_token).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-twilio-signature",
            HeaderValue::from_str(&expected_sig).unwrap(),
        );

        assert!(verify_twilio_signature(&headers, url, body, auth_token).is_ok());
    }

    #[test]
    fn verify_invalid_signature() {
        let url = "https://example.com/webhook/twilio";
        let body = "Body=Hello&From=%2B14155551234";
        let auth_token = "test-auth-token";

        let wrong_sig = compute_signature(url, body, "wrong-token").unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-twilio-signature",
            HeaderValue::from_str(&wrong_sig).unwrap(),
        );

        assert!(verify_twilio_signature(&headers, url, body, auth_token).is_err());
    }

    #[test]
    fn verify_missing_signature() {
        let headers = HeaderMap::new();
        assert!(verify_twilio_signature(&headers, "https://example.com", "", "token").is_err());
    }

    #[test]
    fn verify_invalid_base64_signature() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-twilio-signature",
            HeaderValue::from_static("not-base64!!!"),
        );
        let result =
            verify_twilio_signature(&headers, "https://example.com/wh", "Body=Hi", "token");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("base64") || err_msg.contains("Base64"),
            "error should mention base64, got: {err_msg}"
        );
    }

    #[test]
    fn compute_signature_empty_body() {
        let sig = compute_signature("https://example.com/wh", "", "my-token").unwrap();
        assert!(
            !sig.is_empty(),
            "signature should be non-empty even for empty body"
        );
        // Verify it's valid base64
        base64::engine::general_purpose::STANDARD
            .decode(&sig)
            .expect("signature should be valid base64");
    }

    #[test]
    fn params_sorted_alphabetically() {
        // Ensure param sorting affects signature
        let url = "https://example.com/wh";
        let body1 = "A=1&B=2";
        let body2 = "B=2&A=1";
        let token = "tok";

        // Same params in different order should produce same signature
        assert_eq!(
            compute_signature(url, body1, token).unwrap(),
            compute_signature(url, body2, token).unwrap()
        );
    }

    #[test]
    fn body_with_params_differs_from_empty() {
        // Verify that params are actually included in the signature, so
        // an attacker can't strip params and reuse a signature computed
        // against empty params (the old unwrap_or_default risk).
        let url = "https://example.com/wh";
        let token = "tok";
        let sig_with_params = compute_signature(url, "Key=Value", token).unwrap();
        let sig_empty = compute_signature(url, "", token).unwrap();
        assert_ne!(
            sig_with_params, sig_empty,
            "signatures must differ when params are present vs absent"
        );
    }
}
