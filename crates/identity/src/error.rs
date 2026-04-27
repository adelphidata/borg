//! Typed errors for PKI operations.

use thiserror::Error;

/// Errors emitted by the `borg-pki` crate.
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
