use anyhow::{bail, Result};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const TIMESTAMP_REPLAY_WINDOW_SECS: u64 = crate::constants::SLACK_REPLAY_WINDOW_SECS;

/// Verify a Slack request signature using HMAC-SHA256.
///
/// Slack signs requests with `v0=HMAC-SHA256(signing_secret, "v0:{timestamp}:{body}")`.
/// The signature is in the `X-Slack-Signature` header, and the timestamp is in
/// `X-Slack-Request-Timestamp`.
pub fn verify_slack_signature(headers: &HeaderMap, body: &str, signing_secret: &str) -> Result<()> {
    let timestamp = crate::verify_common::required_header(
        headers,
        "x-slack-request-timestamp",
        "X-Slack-Request-Timestamp",
    )?;

    // Replay protection: reject non-numeric or stale timestamps
    let ts: i64 = timestamp
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid X-Slack-Request-Timestamp: not an integer"))?;
    let now = chrono::Utc::now().timestamp();
    if (now - ts).unsigned_abs() > TIMESTAMP_REPLAY_WINDOW_SECS {
        bail!("Slack request timestamp too old (replay protection)");
    }

    let expected_sig =
        crate::verify_common::required_header(headers, "x-slack-signature", "X-Slack-Signature")?;

    let sig_basestring = format!("v0:{timestamp}:{body}");

    let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
        .map_err(|e| anyhow::anyhow!("HMAC key error: {e}"))?;
    mac.update(sig_basestring.as_bytes());
    let result = mac.finalize();
    let computed = format!("v0={}", hex::encode(result.into_bytes()));

    if !crate::crypto::constant_time_eq(computed.as_bytes(), expected_sig.as_bytes()) {
        bail!("Slack signature verification failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn compute_signature(secret: &str, timestamp: &str, body: &str) -> String {
        let sig_basestring = format!("v0:{timestamp}:{body}");
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(sig_basestring.as_bytes());
        let result = mac.finalize();
        format!("v0={}", hex::encode(result.into_bytes()))
    }

    fn make_headers(timestamp: &str, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_str(timestamp).unwrap(),
        );
        headers.insert(
            "x-slack-signature",
            HeaderValue::from_str(signature).unwrap(),
        );
        headers
    }

    #[test]
    fn valid_signature_passes() {
        let secret = "test-signing-secret";
        let body = r#"{"type":"event_callback","event":{"type":"message"}}"#;
        let timestamp = &chrono::Utc::now().timestamp().to_string();
        let sig = compute_signature(secret, timestamp, body);
        let headers = make_headers(timestamp, &sig);

        assert!(verify_slack_signature(&headers, body, secret).is_ok());
    }

    #[test]
    fn wrong_secret_fails() {
        let body = r#"{"type":"event_callback"}"#;
        let timestamp = &chrono::Utc::now().timestamp().to_string();
        let sig = compute_signature("correct-secret", timestamp, body);
        let headers = make_headers(timestamp, &sig);

        assert!(verify_slack_signature(&headers, body, "wrong-secret").is_err());
    }

    #[test]
    fn tampered_body_fails() {
        let secret = "test-secret";
        let body = r#"{"type":"event_callback"}"#;
        let timestamp = &chrono::Utc::now().timestamp().to_string();
        let sig = compute_signature(secret, timestamp, body);
        let headers = make_headers(timestamp, &sig);

        assert!(verify_slack_signature(&headers, "tampered body", secret).is_err());
    }

    #[test]
    fn old_timestamp_rejected() {
        let secret = "test-secret";
        let body = "hello";
        // 10 minutes ago
        let old_ts = (chrono::Utc::now().timestamp() - 600).to_string();
        let sig = compute_signature(secret, &old_ts, body);
        let headers = make_headers(&old_ts, &sig);

        let result = verify_slack_signature(&headers, body, secret);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("timestamp too old"));
    }

    #[test]
    fn missing_timestamp_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-signature", HeaderValue::from_static("v0=abc123"));

        assert!(verify_slack_signature(&headers, "body", "secret").is_err());
    }

    #[test]
    fn missing_signature_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_static("1234567890"),
        );

        assert!(verify_slack_signature(&headers, "body", "secret").is_err());
    }

    #[test]
    fn non_numeric_timestamp_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_static("not-a-number"),
        );
        headers.insert("x-slack-signature", HeaderValue::from_static("v0=abc"));

        let result = verify_slack_signature(&headers, "body", "secret");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not an integer"));
    }
}
