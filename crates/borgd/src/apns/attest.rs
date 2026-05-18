//! Apple App Attest verifier — **stub implementation.**
//!
//! The full verifier needs:
//! 1. CBOR decode of the attestation object (`fmt: "apple-appattest"`).
//! 2. x5c chain verification against Apple's App Attest root CA.
//! 3. Nonce extraction from credCert OID `1.2.840.113635.100.8.2`,
//!    matching `sha256(authData || sha256(challenge))`.
//! 4. RP ID hash check: `authData[0..32] == sha256(bundle_id)`.
//! 5. UP flag + counter check on `authData[32..]`.
//! 6. CredentialId match: `authData[55..87] == key_id`.
//! 7. P-256 SubjectPublicKeyInfo extraction from credCert.
//!
//! Today this returns `AttestError::NotImplemented`. The surrounding
//! plumbing (gRPC registration, challenge store, DB persistence,
//! 410-invalidation) is real so the production verifier slots in
//! without changing any caller signatures.

use thiserror::Error;

/// Errors the production verifier can surface. `NotImplemented` is the
/// only variant returned today; the rest are reserved so the gRPC layer
/// can already pattern-match on them and produce stable error codes.
#[derive(Debug, Error)]
pub enum AttestError {
    /// The verifier hasn't been wired yet. Stub-only path.
    #[error("App Attest verifier not implemented — feature pending; see docs/ROADMAP.md")]
    NotImplemented,
    /// CBOR decode of the attestation blob failed.
    #[error("CBOR decode failed: {0}")]
    CborDecode(String),
    /// The challenge in the assertion does not match the server-issued one.
    #[error("challenge mismatch")]
    ChallengeMismatch,
    /// Server-issued challenge has expired (>5 minutes old).
    #[error("challenge expired")]
    ChallengeExpired,
    /// The x5c certificate chain does not chain to Apple's App Attest root.
    #[error("certificate chain does not chain to Apple App Attest root")]
    CertChainInvalid,
    /// The signature on the credCert (or its leaf) is invalid.
    #[error("invalid signature")]
    InvalidSignature,
    /// `authData[0..32]` does not equal `sha256(bundle_id)`.
    #[error("RP ID hash mismatch (bundle_id does not match attestation)")]
    RpIdMismatch,
    /// `authData[55..87]` does not equal the supplied `key_id`.
    #[error("credentialId does not match key_id")]
    CredentialIdMismatch,
}

/// What the production verifier returns on success. Today this struct is
/// never constructed in production code (the verifier short-circuits to
/// `NotImplemented`); it exists so the DB layer and gRPC handlers can
/// already use it and the verifier-swap is a single-file change.
#[derive(Debug, Clone)]
pub struct VerifiedAttestation {
    /// SHA-256 of the App Attest public key — same value the iOS client
    /// sent as `key_id` when generating the attestation. Stored on the
    /// `apns_device_tokens` row so future assertions can be re-verified
    /// without rerunning attestation.
    pub key_id: Vec<u8>,
    /// DER-encoded P-256 SubjectPublicKeyInfo from the credCert. The
    /// `app_attest_pub_key_der` BLOB column receives this verbatim.
    pub public_key_der: Vec<u8>,
}

/// Verify an App Attest attestation. **Stub implementation** — returns
/// `AttestError::NotImplemented`. See module docs for the steps the
/// production verifier will perform.
///
/// Argument names match the production signature so the swap is a
/// single-file change: the public API stays stable as the feature lands.
pub fn verify_attestation(
    _bundle_id: &str,
    _key_id: &[u8],
    _attestation_cbor: &[u8],
    _challenge: &[u8],
) -> Result<VerifiedAttestation, AttestError> {
    Err(AttestError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_verifier_returns_not_implemented_with_distinctive_error_message() {
        // Why this test exists: the production verifier is a follow-up.
        // Until then, every caller path (gRPC `RegisterDevice`, the
        // mock-client harness) must observe `NotImplemented` rather
        // than a misleading `Ok(_)` or a generic error. The message
        // must point operators at the roadmap so a real device register
        // attempt produces an actionable response.
        let err =
            verify_attestation("com.borg.ios", &[0xAB; 32], &[0xCD; 64], &[0xEF; 32]).unwrap_err();
        assert!(
            matches!(err, AttestError::NotImplemented),
            "stub must return NotImplemented; got {err:?}"
        );
        let msg = format!("{err}");
        assert!(msg.contains("not implemented"), "{msg}");
        assert!(msg.contains("ROADMAP"), "msg must steer to roadmap: {msg}");
    }

    #[test]
    fn error_variants_implement_display_with_distinct_messages() {
        // Stability guard: the gRPC layer maps each variant to a
        // Status code. Collapsing two variants to the same string would
        // hide which check failed when debugging real device pairings.
        let variants = [
            AttestError::NotImplemented,
            AttestError::CborDecode("x".into()),
            AttestError::ChallengeMismatch,
            AttestError::ChallengeExpired,
            AttestError::CertChainInvalid,
            AttestError::InvalidSignature,
            AttestError::RpIdMismatch,
            AttestError::CredentialIdMismatch,
        ];
        let msgs: Vec<String> = variants.iter().map(|e| format!("{e}")).collect();
        let unique: std::collections::HashSet<_> = msgs.iter().collect();
        assert_eq!(
            unique.len(),
            msgs.len(),
            "every variant must Display to a distinct string; got {msgs:?}"
        );
    }
}
