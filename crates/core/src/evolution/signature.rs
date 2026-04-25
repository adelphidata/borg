//! Ed25519 event signing for evolution events (v3+).
//!
//! Replaces the v1/v2 HMAC chain for newly-recorded events. The private key
//! lives in the OS keychain (see [`crate::evolution::keychain`]); the public
//! key is stored in the `device_keys` table and referenced from each
//! `evolution_events` row via `pubkey_id`. Legacy rows where `pubkey_id IS
//! NULL` continue to verify via HMAC — see [`crate::evolution::hmac`].
//!
//! The hashed payload format mirrors the HMAC v2 scheme so a future migration
//! can re-sign legacy rows without changing the canonicalisation rules.

use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SIGNATURE_LENGTH};

/// Build the canonical payload bytes hashed for signing/verification.
/// Field order and separators MUST stay stable — changing this breaks every
/// previously-signed row. Mirrors the HMAC v2 layout.
fn canonical_payload(
    prev_signature: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    metadata: &str,
    created_at: i64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        prev_signature.len()
            + event_type.len()
            + archetype.len()
            + source.len()
            + metadata.len()
            + 16,
    );
    buf.extend_from_slice(prev_signature.as_bytes());
    buf.extend_from_slice(event_type.as_bytes());
    buf.extend_from_slice(&xp_delta.to_le_bytes());
    buf.extend_from_slice(archetype.as_bytes());
    buf.extend_from_slice(source.as_bytes());
    buf.extend_from_slice(metadata.as_bytes());
    buf.extend_from_slice(&created_at.to_le_bytes());
    buf
}

/// Sign an evolution event payload with the install's signing key. Returns
/// a hex-encoded signature suitable for storing in the `hmac` column.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_event(
    key: &SigningKey,
    prev_signature: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    metadata: &str,
    created_at: i64,
) -> String {
    let payload = canonical_payload(
        prev_signature,
        event_type,
        xp_delta,
        archetype,
        source,
        metadata,
        created_at,
    );
    let sig = key.sign(&payload);
    hex_encode(&sig.to_bytes())
}

/// Verify a hex-encoded Ed25519 signature against an event's payload.
/// Returns false on any failure (decode, length, signature parse, verify).
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_event(
    pubkey: &VerifyingKey,
    signature_hex: &str,
    prev_signature: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    metadata: &str,
    created_at: i64,
) -> bool {
    let Some(sig_bytes) = hex_decode(signature_hex) else {
        return false;
    };
    if sig_bytes.len() != SIGNATURE_LENGTH {
        return false;
    }
    let mut arr = [0u8; SIGNATURE_LENGTH];
    arr.copy_from_slice(&sig_bytes);
    let sig = Signature::from_bytes(&arr);
    let payload = canonical_payload(
        prev_signature,
        event_type,
        xp_delta,
        archetype,
        source,
        metadata,
        created_at,
    );
    pubkey.verify(&payload, &sig).is_ok()
}

/// Decode a verifying key from raw 32-byte public key material as stored in
/// `device_keys.public_key`.
pub(crate) fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey> {
    if bytes.len() != 32 {
        anyhow::bail!(
            "device_keys.public_key must be 32 bytes, got {}",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes);
    VerifyingKey::from_bytes(&arr).context("invalid Ed25519 public key bytes")
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_key() -> SigningKey {
        use rand::Rng;
        let mut seed = [0u8; 32];
        rand::rng().fill(&mut seed[..]);
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let key = fresh_key();
        let pk = key.verifying_key();
        let sig = sign_event(
            &key,
            "0",
            "xp_gain",
            2,
            "ops",
            "kubectl",
            "{}",
            1_700_000_000,
        );
        assert!(verify_event(
            &pk,
            &sig,
            "0",
            "xp_gain",
            2,
            "ops",
            "kubectl",
            "{}",
            1_700_000_000
        ));
    }

    #[test]
    fn verify_fails_when_payload_modified() {
        let key = fresh_key();
        let pk = key.verifying_key();
        let sig = sign_event(
            &key,
            "0",
            "xp_gain",
            2,
            "ops",
            "kubectl",
            "{}",
            1_700_000_000,
        );
        // xp_delta tampered
        assert!(!verify_event(
            &pk,
            &sig,
            "0",
            "xp_gain",
            99,
            "ops",
            "kubectl",
            "{}",
            1_700_000_000
        ));
        // archetype tampered
        assert!(!verify_event(
            &pk,
            &sig,
            "0",
            "xp_gain",
            2,
            "marketer",
            "kubectl",
            "{}",
            1_700_000_000
        ));
        // prev_signature tampered (chain break)
        assert!(!verify_event(
            &pk,
            &sig,
            "deadbeef",
            "xp_gain",
            2,
            "ops",
            "kubectl",
            "{}",
            1_700_000_000
        ));
    }

    #[test]
    fn verify_fails_with_different_pubkey() {
        let signer = fresh_key();
        let other = fresh_key();
        let sig = sign_event(&signer, "0", "xp_gain", 1, "ops", "src", "", 100);
        assert!(!verify_event(
            &other.verifying_key(),
            &sig,
            "0",
            "xp_gain",
            1,
            "ops",
            "src",
            "",
            100
        ));
    }

    #[test]
    fn malformed_signature_does_not_panic() {
        let key = fresh_key();
        let pk = key.verifying_key();
        assert!(!verify_event(
            &pk, "not-hex", "0", "xp_gain", 1, "ops", "src", "", 100
        ));
        assert!(!verify_event(
            &pk, "abcd", "0", "xp_gain", 1, "ops", "src", "", 100
        )); // too short
        assert!(!verify_event(
            &pk, "zz", "0", "xp_gain", 1, "ops", "src", "", 100
        )); // bad nibble
    }

    #[test]
    fn hex_roundtrip_preserves_bytes() {
        for input in [&[0u8, 1, 254, 255][..], &[][..], &[0xab, 0xcd, 0xef][..]] {
            let encoded = hex_encode(input);
            let decoded = hex_decode(&encoded).expect("valid hex");
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn verifying_key_from_bytes_rejects_wrong_length() {
        assert!(verifying_key_from_bytes(&[0u8; 16]).is_err());
        assert!(verifying_key_from_bytes(&[0u8; 64]).is_err());
        let key = fresh_key();
        let bytes = key.verifying_key().to_bytes();
        assert!(verifying_key_from_bytes(&bytes).is_ok());
    }
}
