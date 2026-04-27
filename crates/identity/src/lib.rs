//! Borg internal PKI: CA ops, cert lifecycle, OS keychain wrapping.
//!
//! Daemons mint a self-signed Ed25519 root CA on first start, then issue 90-day
//! server and client certificates. Private keys live in the OS keychain
//! (macOS Keychain / Linux Secret Service / Windows credential store) and never
//! touch disk in plaintext. Cert PEMs and metadata are persisted by callers in
//! the `internal_ca_certs` table — this crate is DB-free.
#![warn(missing_docs)]

pub mod ca;
pub mod cert;
pub mod error;
pub mod keychain;

pub use ca::{ClientCertParams, IssuedCert, RootCa, ServerCertParams};
pub use cert::{
    expires_within, fingerprint_of_pem, fingerprint_sha256, parse_cert_metadata, CertMetadata,
};
pub use error::PkiError;
pub use keychain::{KekStore, KeychainStore, MemoryKekStore};
