//! Certificate parsing and fingerprinting utilities.

use crate::error::PkiError;
use sha2::{Digest, Sha256};
use x509_parser::prelude::*;

/// Compute a SHA-256 fingerprint over a DER-encoded certificate.
///
/// Returns a lowercase hex string (no colons) — matches `openssl x509 -fingerprint -sha256`
/// after stripping colons and lowercasing.
pub fn fingerprint_sha256(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let digest = hasher.finalize();
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(&mut acc, "{b:02x}");
        acc
    })
}

/// Parsed metadata from an X.509 certificate, used for renewal checks and audit logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertMetadata {
    /// Common-name from the subject DN, when present.
    pub subject_cn: Option<String>,
    /// First URI from subject alt name, when present (we use this for device-id binding).
    pub san_uri: Option<String>,
    /// Unix timestamp of `notBefore`.
    pub not_before: i64,
    /// Unix timestamp of `notAfter`.
    pub not_after: i64,
    /// Lowercase-hex SHA-256 fingerprint over the DER bytes parsed.
    pub fingerprint: String,
}

/// Parse cert metadata from DER bytes.
pub fn parse_cert_metadata(cert_der: &[u8]) -> Result<CertMetadata, PkiError> {
    let (_, cert) = X509Certificate::from_der(cert_der)
        .map_err(|e| PkiError::Parse(format!("der parse: {e}")))?;

    let subject_cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok().map(std::string::ToString::to_string));

    let san_uri = cert
        .extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => {
                san.general_names.iter().find_map(|gn| match gn {
                    GeneralName::URI(uri) => Some((*uri).to_string()),
                    _ => None,
                })
            }
            _ => None,
        });

    Ok(CertMetadata {
        subject_cn,
        san_uri,
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        fingerprint: fingerprint_sha256(cert_der),
    })
}

/// SHA-256 fingerprint of the first certificate in a PEM bundle.
///
/// Returns lowercase-hex (matches [`fingerprint_sha256`]). Errors if the
/// bundle contains no `CERTIFICATE` block or the embedded DER fails to parse.
pub fn fingerprint_of_pem(cert_pem: &[u8]) -> Result<String, PkiError> {
    let mut cursor = std::io::Cursor::new(cert_pem);
    let first = rustls_pemfile::certs(&mut cursor)
        .next()
        .ok_or_else(|| PkiError::Parse("no CERTIFICATE block in PEM bundle".to_string()))?;
    let der = first.map_err(|e| PkiError::Parse(format!("pem read: {e}")))?;
    Ok(fingerprint_sha256(&der))
}

/// True iff the cert expires within `days` from `now`.
pub fn expires_within(meta: &CertMetadata, now_unix: i64, days: u32) -> bool {
    let cutoff = now_unix.saturating_add(i64::from(days) * 86_400);
    meta.not_after <= cutoff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_test_cert(not_before_offset: i64, not_after_offset: i64) -> Vec<u8> {
        let mut params =
            rcgen::CertificateParams::new(vec!["test.local".to_string()]).expect("rcgen params");
        let now = ::time::OffsetDateTime::now_utc();
        params.not_before = now + ::time::Duration::seconds(not_before_offset);
        params.not_after = now + ::time::Duration::seconds(not_after_offset);
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("test-device".to_string()),
        );
        let key = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key).expect("self_signed");
        cert.der().to_vec()
    }

    #[test]
    fn fingerprint_is_deterministic_64_lowercase_hex() {
        let der = issue_test_cert(-60, 86_400);
        let a = fingerprint_sha256(&der);
        let b = fingerprint_sha256(&der);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn fingerprint_changes_on_byte_flip() {
        let der = issue_test_cert(-60, 86_400);
        let mut tampered = der.clone();
        tampered[0] ^= 0x01;
        assert_ne!(fingerprint_sha256(&der), fingerprint_sha256(&tampered));
    }

    #[test]
    fn parse_metadata_extracts_cn_and_validity() {
        let der = issue_test_cert(-60, 86_400 * 90);
        let meta = parse_cert_metadata(&der).expect("parse");
        assert_eq!(meta.subject_cn.as_deref(), Some("test-device"));
        assert!(meta.not_after > meta.not_before);
        assert_eq!(meta.fingerprint.len(), 64);
    }

    #[test]
    fn expires_within_boundaries() {
        let now = 1_700_000_000_i64;
        let meta = CertMetadata {
            subject_cn: None,
            san_uri: None,
            not_before: now - 86_400,
            not_after: now + 13 * 86_400,
            fingerprint: "x".into(),
        };
        assert!(expires_within(&meta, now, 14));
        assert!(expires_within(&meta, now, 13));
        assert!(!expires_within(&meta, now, 12));
    }

    fn issue_test_cert_pem() -> (Vec<u8>, String) {
        let mut params =
            rcgen::CertificateParams::new(vec!["test.local".to_string()]).expect("rcgen params");
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("test-device".to_string()),
        );
        let key = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key).expect("self_signed");
        let der = cert.der().to_vec();
        let pem = cert.pem();
        (der, pem)
    }

    #[test]
    fn fingerprint_of_pem_matches_der_fingerprint() {
        let (der, pem) = issue_test_cert_pem();
        let want = fingerprint_sha256(&der);
        let got = fingerprint_of_pem(pem.as_bytes()).expect("pem fingerprint");
        assert_eq!(got, want);
    }

    #[test]
    fn fingerprint_of_pem_errors_when_no_cert_block() {
        let err =
            fingerprint_of_pem(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
                .expect_err("must fail");
        assert!(matches!(err, PkiError::Parse(_)));
    }

    #[test]
    fn parse_invalid_der_returns_parse_error() {
        let result = parse_cert_metadata(&[0u8; 32]);
        assert!(matches!(result, Err(PkiError::Parse(_))));
    }
}
