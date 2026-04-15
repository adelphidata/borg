use anyhow::Result;
use axum::http::HeaderMap;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Verify a Discord interaction request using Ed25519 signature verification.
///
/// Discord signs each interaction request with Ed25519. The signature and timestamp
/// are provided in the `X-Signature-Ed25519` and `X-Signature-Timestamp` headers.
/// The signed message is `timestamp + body`.
///
/// Discord handles replay protection server-side via the signed timestamp,
/// so no additional replay window check is needed here.
pub fn verify_discord_signature(
    headers: &HeaderMap,
    body: &str,
    public_key_hex: &str,
) -> Result<()> {
    // Parse the public key from hex
    let key_bytes = hex::decode(public_key_hex)
        .map_err(|e| anyhow::anyhow!("Invalid Discord public key hex: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Discord public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("Invalid Discord public key: {e}"))?;

    // Extract headers
    let signature_hex = crate::verify_common::required_header(
        headers,
        "x-signature-ed25519",
        "X-Signature-Ed25519",
    )?;

    let timestamp = crate::verify_common::required_header(
        headers,
        "x-signature-timestamp",
        "X-Signature-Timestamp",
    )?;

    // Parse the signature from hex
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
            "x-signature-ed25519",
            HeaderValue::from_str(&sig_hex).unwrap(),
        );
        headers.insert(
            "x-signature-timestamp",
            HeaderValue::from_str(timestamp).unwrap(),
        );

        let public_key_hex = hex::encode(signing_key.verifying_key().to_bytes());
        (headers, public_key_hex)
    }

    #[test]
    fn valid_signature_passes() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let body = r#"{"type":1}"#;
        let timestamp = "1234567890";

        let (headers, public_key_hex) = make_signed_request(&signing_key, timestamp, body);
        assert!(verify_discord_signature(&headers, body, &public_key_hex).is_ok());
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
            "x-signature-timestamp",
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
        headers.insert("x-signature-ed25519", HeaderValue::from_static("aabbccdd"));

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
        headers.insert("x-signature-ed25519", HeaderValue::from_static("aabb"));
        headers.insert("x-signature-timestamp", HeaderValue::from_static("12345"));

        let result = verify_discord_signature(&headers, "body", "not-hex!");
        assert!(result.is_err());
    }

    #[test]
    fn invalid_public_key_length_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-signature-ed25519", HeaderValue::from_static("aabb"));
        headers.insert("x-signature-timestamp", HeaderValue::from_static("12345"));

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
            "x-signature-timestamp",
            HeaderValue::from_static("9999999999"),
        );

        assert!(verify_discord_signature(&headers, body, &public_key_hex).is_err());
    }
}
