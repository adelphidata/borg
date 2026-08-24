use anyhow::{bail, Result};
use axum::http::HeaderMap;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::verify_common::headers as hdr;

const REPLAY_WINDOW_SECS: u64 = crate::constants::DISCORD_REPLAY_WINDOW_SECS;

/// Verify a Discord interaction request using Ed25519 signature verification.
///
/// Discord signs each interaction request with Ed25519. The signature and timestamp
/// are provided in the `X-Signature-Ed25519` and `X-Signature-Timestamp` headers.
/// The signed message is `timestamp + body`.
///
/// After the signature verifies, the signed timestamp is checked against a
/// freshness window: the signature proves the timestamp is authentic, but does
/// not stop a captured, validly signed request from being replayed to our
/// endpoint later. Rejecting stale timestamps closes that gap.
pub fn verify_discord_signature(
    headers: &HeaderMap,
    body: &str,
    public_key_hex: &str,
) -> Result<()> {
    verify_discord_signature_at(headers, body, public_key_hex, chrono::Utc::now().timestamp())
}

/// Inner verifier with an injected clock so the replay-window boundary is
/// testable deterministically.
fn verify_discord_signature_at(
    headers: &HeaderMap,
    body: &str,
    public_key_hex: &str,
    now: i64,
) -> Result<()> {
    let key_bytes = hex::decode(public_key_hex)
        .map_err(|e| anyhow::anyhow!("Invalid Discord public key hex: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Discord public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("Invalid Discord public key: {e}"))?;

    let signature_hex = crate::verify_common::required_header(
        headers,
        hdr::DISCORD_SIGNATURE,
        "X-Signature-Ed25519",
    )?;

    let timestamp = crate::verify_common::required_header(
        headers,
        hdr::DISCORD_TIMESTAMP,
        "X-Signature-Timestamp",
    )?;

    let sig_bytes =
        hex::decode(signature_hex).map_err(|e| anyhow::anyhow!("Invalid signature hex: {e}"))?;
    let sig_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Signature must be 64 bytes"))?;
    let signature = Signature::from_bytes(&sig_array);

    // Verify: signed message is timestamp + body
    let message = format!("{timestamp}{body}");
    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("Discord signature verification failed"))?;

    // Replay protection: the timestamp is now trusted (it was signed). Reject it
    // if it falls outside the freshness window, so an old captured request can't
    // be replayed.
    let ts: i64 = timestamp
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid X-Signature-Timestamp: not an integer"))?;
    if (now - ts).unsigned_abs() > REPLAY_WINDOW_SECS {
        bail!("Discord request timestamp too old (replay protection)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_signed_request(
        signing_key: &SigningKey,
        timestamp: &str,
        body: &str,
    ) -> (HeaderMap, String) {
        let message = format!("{timestamp}{body}");
        let signature = signing_key.sign(message.as_bytes());
        let sig_hex = hex::encode(signature.to_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            hdr::DISCORD_SIGNATURE,
            HeaderValue::from_str(&sig_hex).unwrap(),
        );
        headers.insert(
            hdr::DISCORD_TIMESTAMP,
            HeaderValue::from_str(timestamp).unwrap(),
        );

        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        (headers, public_key_hex)
    }

    #[test]
    fn valid_signature_passes() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;
        // Fresh timestamp — the freshness window rejects stale ones.
        let timestamp = chrono::Utc::now().timestamp().to_string();

        let (headers, public_key_hex) = make_signed_request(&signing_key, &timestamp, body);
        assert!(verify_discord_signature(&headers, body, &public_key_hex).is_ok());
    }

    #[test]
    fn old_signed_timestamp_rejected() {
        // A validly signed but stale request (captured and replayed) must be
        // rejected by the freshness window even though the signature verifies.
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;
        let old_ts = (chrono::Utc::now().timestamp() - 3600).to_string();

        let (headers, public_key_hex) = make_signed_request(&signing_key, &old_ts, body);
        let result = verify_discord_signature(&headers, body, &public_key_hex);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("too old"),
            "expected replay-protection rejection"
        );
    }

    #[test]
    fn replay_window_boundary_is_inclusive() {
        // Spec: a signed timestamp exactly REPLAY_WINDOW_SECS old (or in
        // the future) is still inside the window; one second beyond it is
        // rejected. The injected clock makes the boundary deterministic.
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;
        let ts: i64 = 1_700_000_000;
        let (headers, public_key_hex) = make_signed_request(&signing_key, &ts.to_string(), body);
        let window = REPLAY_WINDOW_SECS as i64;

        for (now, ok) in [
            (ts + window, true),
            (ts - window, true),
            (ts + window + 1, false),
            (ts - window - 1, false),
        ] {
            let result = verify_discord_signature_at(&headers, body, &public_key_hex, now);
            assert_eq!(
                result.is_ok(),
                ok,
                "skew {} must be {}",
                now - ts,
                if ok { "accepted" } else { "rejected" }
            );
        }
    }

    #[test]
    fn wrong_key_fails() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let wrong_key = SigningKey::from_bytes(&[2u8; 32]);
        let body = r#"{"type":1}"#;
        let timestamp = "1234567890";

        let (headers, _) = make_signed_request(&signing_key, timestamp, body);
        let wrong_pub_hex = hex::encode(wrong_key.verifying_key().to_bytes());

        assert!(verify_discord_signature(&headers, body, &wrong_pub_hex).is_err());
    }

    #[test]
    fn tampered_body_fails() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;
        let timestamp = "1234567890";

        let (headers, public_key_hex) = make_signed_request(&signing_key, timestamp, body);
        assert!(verify_discord_signature(&headers, "tampered", &public_key_hex).is_err());
    }

    #[test]
    fn missing_signature_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            hdr::DISCORD_TIMESTAMP,
            HeaderValue::from_static("1234567890"),
        );

        let key = SigningKey::from_bytes(&[1u8; 32]);
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        let result = verify_discord_signature(&headers, "body", &pub_hex);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing X-Signature-Ed25519"));
    }

    #[test]
    fn missing_timestamp_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(hdr::DISCORD_SIGNATURE, HeaderValue::from_static("aabbccdd"));

        let key = SigningKey::from_bytes(&[1u8; 32]);
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        let result = verify_discord_signature(&headers, "body", &pub_hex);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing X-Signature-Timestamp"));
    }

    #[test]
    fn invalid_public_key_hex_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(hdr::DISCORD_SIGNATURE, HeaderValue::from_static("aabb"));
        headers.insert(hdr::DISCORD_TIMESTAMP, HeaderValue::from_static("12345"));

        let result = verify_discord_signature(&headers, "body", "not-hex!");
        assert!(result.is_err());
    }

    #[test]
    fn invalid_public_key_length_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(hdr::DISCORD_SIGNATURE, HeaderValue::from_static("aabb"));
        headers.insert(hdr::DISCORD_TIMESTAMP, HeaderValue::from_static("12345"));

        // Valid hex but wrong length (16 bytes instead of 32)
        let result = verify_discord_signature(&headers, "body", "aabbccddaabbccddaabbccddaabbccdd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32 bytes"));
    }

    #[test]
    fn different_timestamp_fails() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;

        let (mut headers, public_key_hex) = make_signed_request(&signing_key, "1234567890", body);
        // Replace timestamp with a different value — signature won't match
        headers.insert(
            hdr::DISCORD_TIMESTAMP,
            HeaderValue::from_static("9999999999"),
        );

        assert!(verify_discord_signature(&headers, body, &public_key_hex).is_err());
    }
}
