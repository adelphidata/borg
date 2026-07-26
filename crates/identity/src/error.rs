//! Typed errors for PKI operations.

use thiserror::Error;

/// Errors emitted by PKI operations (CA, cert, keychain).
#[derive(Debug, Error)]
pub enum PkiError {
    /// Failed to mint or parse a certificate.
    #[error("certificate operation failed: {0}")]
    Cert(String),

    /// Failed to parse a certificate from PEM/DER.
    #[error("invalid certificate encoding: {0}")]
    Parse(String),

    /// Keychain backend error.
    #[error("keychain backend error: {0}")]
    Keychain(String),

    /// Requested cert/key not present.
    #[error("not found: {0}")]
    NotFound(String),

    /// Cryptographic key generation failed.
    #[error("key generation failed: {0}")]
    KeyGen(String),
}

impl From<rcgen::Error> for PkiError {
    fn from(err: rcgen::Error) -> Self {
        Self::Cert(err.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_distinguish_each_variant() {
        // The verbatim error-message prefixes are part of the operator
        // surface: borg's TUI / `borg device pair` flows grep these to
        // route the "run pair first" hint. Lock the prefixes so a
        // refactor that re-words them breaks here, not in the UX.
        let cases: &[(PkiError, &str)] = &[
            (
                PkiError::Cert("c".into()),
                "certificate operation failed: c",
            ),
            (
                PkiError::Parse("p".into()),
                "invalid certificate encoding: p",
            ),
            (PkiError::Keychain("k".into()), "keychain backend error: k"),
            (PkiError::NotFound("n".into()), "not found: n"),
            (PkiError::KeyGen("g".into()), "key generation failed: g"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), *expected);
        }
    }

    #[test]
    fn rcgen_error_converts_to_cert_variant() {
        // rcgen::Error has no public constructor, so trigger a real one by
        // parsing a malformed CA PEM.
        let bogus_pem = "-----BEGIN CERTIFICATE-----\nnot valid\n-----END CERTIFICATE-----\n";
        let rcgen_err = rcgen::CertificateParams::from_ca_cert_pem(bogus_pem)
            .expect_err("malformed PEM must fail");
        let converted: PkiError = rcgen_err.into();
        assert!(
            matches!(converted, PkiError::Cert(_)),
            "rcgen errors must map to PkiError::Cert, got {converted:?}"
        );
        // And the Display must surface the operator-facing prefix so an
        // operator triaging logs sees "certificate operation failed: …"
        // rather than a bare rcgen string.
        assert!(converted
            .to_string()
            .starts_with("certificate operation failed:"));
    }
}
