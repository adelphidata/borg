//! CA operations: mint root CA, server certs, and client certs (Ed25519).

use crate::error::PkiError;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use time::{Duration, OffsetDateTime};

/// A self-signed root CA — PEM cert + Ed25519 keypair.
#[derive(Debug)]
pub struct RootCa {
    /// PEM-encoded CA certificate.
    pub cert_pem: String,
    /// PEM-encoded PKCS#8 private key (Ed25519). Caller MUST move this to the keychain.
    pub key_pkcs8_pem: String,
    /// DER-encoded cert (for fingerprint).
    pub cert_der: Vec<u8>,
}

/// Parameters for issuing a server certificate.
#[derive(Debug, Clone)]
pub struct ServerCertParams<'a> {
    /// Subject CN, e.g. `"borgd"`.
    pub common_name: &'a str,
    /// DNS names to embed in SAN.
    pub san_dns: &'a [String],
    /// Validity in days.
    pub validity_days: u32,
}

/// Parameters for issuing a client (device) certificate.
#[derive(Debug, Clone)]
pub struct ClientCertParams<'a> {
    /// Subject CN — typically the device label.
    pub device_label: &'a str,
    /// Device id placed in `URI:borg-device:{id}` SAN.
    pub device_id: &'a str,
    /// PEM-encoded keypair provided by the client during pairing.
    pub client_public_key_pem: &'a str,
    /// Validity in days.
    pub validity_days: u32,
}

/// A signed certificate ready to hand back to the requester.
#[derive(Debug)]
pub struct IssuedCert {
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// DER-encoded certificate (for fingerprinting).
    pub cert_der: Vec<u8>,
    /// `notBefore` timestamp (unix seconds).
    pub not_before: i64,
    /// `notAfter` timestamp (unix seconds).
    pub not_after: i64,
}

fn name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn.to_string());
    dn
}

/// Generate a fresh root CA (Ed25519, 5-year validity).
pub fn generate_root_ca(common_name: &str) -> Result<RootCa, PkiError> {
    let mut params = CertificateParams::default();
    params.distinguished_name = name(common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + Duration::days(365 * 5);

    let key =
        KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(|e| PkiError::KeyGen(e.to_string()))?;
    let cert = params.self_signed(&key)?;

    Ok(RootCa {
        cert_pem: cert.pem(),
        key_pkcs8_pem: key.serialize_pem(),
        cert_der: cert.der().to_vec(),
    })
}

/// Issue a server cert signed by the supplied root CA.
pub fn issue_server_cert(
    root_cert_pem: &str,
    root_key_pkcs8_pem: &str,
    params_in: &ServerCertParams<'_>,
) -> Result<(IssuedCert, String), PkiError> {
    let issuer_key =
        KeyPair::from_pem(root_key_pkcs8_pem).map_err(|e| PkiError::KeyGen(e.to_string()))?;
    let issuer_params = CertificateParams::from_ca_cert_pem(root_cert_pem)
        .map_err(|e| PkiError::Parse(e.to_string()))?;
    let issuer_cert = issuer_params.self_signed(&issuer_key)?;

    let mut params = CertificateParams::default();
    params.distinguished_name = name(params_in.common_name);
    let mut sans: Vec<SanType> = Vec::with_capacity(params_in.san_dns.len());
    for dns in params_in.san_dns {
        let ia5 = rcgen::Ia5String::try_from(dns.clone())
            .map_err(|e| PkiError::Cert(format!("san dns {dns}: {e}")))?;
        sans.push(SanType::DnsName(ia5));
    }
    params.subject_alt_names = sans;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + Duration::days(i64::from(params_in.validity_days));

    let key =
        KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(|e| PkiError::KeyGen(e.to_string()))?;
    let not_before = params.not_before.unix_timestamp();
    let not_after = params.not_after.unix_timestamp();
    let cert = params.signed_by(&key, &issuer_cert, &issuer_key)?;

    Ok((
        IssuedCert {
            cert_pem: cert.pem(),
            cert_der: cert.der().to_vec(),
            not_before,
            not_after,
        },
        key.serialize_pem(),
    ))
}

/// Issue a client cert binding the supplied client public key to a device id.
pub fn issue_client_cert(
    root_cert_pem: &str,
    root_key_pkcs8_pem: &str,
    params_in: &ClientCertParams<'_>,
) -> Result<IssuedCert, PkiError> {
    let issuer_key =
        KeyPair::from_pem(root_key_pkcs8_pem).map_err(|e| PkiError::KeyGen(e.to_string()))?;
    let issuer_params = CertificateParams::from_ca_cert_pem(root_cert_pem)
        .map_err(|e| PkiError::Parse(e.to_string()))?;
    let issuer_cert = issuer_params.self_signed(&issuer_key)?;

    let client_pubkey = KeyPair::from_pem(params_in.client_public_key_pem)
        .map_err(|e| PkiError::KeyGen(format!("client pubkey parse: {e}")))?;

    let mut params = CertificateParams::default();
    params.distinguished_name = name(params_in.device_label);
    let san_uri = format!("borg-device:{}", params_in.device_id);
    let san_ia5 = rcgen::Ia5String::try_from(san_uri.clone())
        .map_err(|e| PkiError::Cert(format!("san uri {san_uri}: {e}")))?;
    params.subject_alt_names = vec![SanType::URI(san_ia5)];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + Duration::days(i64::from(params_in.validity_days));

    let not_before = params.not_before.unix_timestamp();
    let not_after = params.not_after.unix_timestamp();
    let cert = params.signed_by(&client_pubkey, &issuer_cert, &issuer_key)?;

    Ok(IssuedCert {
        cert_pem: cert.pem(),
        cert_der: cert.der().to_vec(),
        not_before,
        not_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert::parse_cert_metadata;

    #[test]
    fn root_ca_round_trips_through_pem() {
        let ca = generate_root_ca("borg-test-ca").expect("ca");
        assert!(ca.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(ca.key_pkcs8_pem.contains("PRIVATE KEY"));
        let key = KeyPair::from_pem(&ca.key_pkcs8_pem).expect("reparse");
        let params = CertificateParams::from_ca_cert_pem(&ca.cert_pem).expect("ca params");
        let _re = params.self_signed(&key).expect("re-issue");
    }

    #[test]
    fn server_cert_signed_by_ca_has_correct_validity() {
        let ca = generate_root_ca("borg-test-ca").expect("ca");
        let dns = vec!["localhost".to_string()];
        let p = ServerCertParams {
            common_name: "borgd",
            san_dns: &dns,
            validity_days: 90,
        };
        let (issued, _key_pem) =
            issue_server_cert(&ca.cert_pem, &ca.key_pkcs8_pem, &p).expect("server cert");
        let meta = parse_cert_metadata(&issued.cert_der).expect("parse");
        let span_days = (meta.not_after - meta.not_before) / 86_400;
        assert!(
            (89..=91).contains(&span_days),
            "span_days = {span_days}"
        );
        assert_eq!(meta.subject_cn.as_deref(), Some("borgd"));
    }

    #[test]
    fn client_cert_binds_device_id_in_san() {
        let ca = generate_root_ca("borg-test-ca").expect("ca");
        let client_kp = KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("kp");
        let client_pub_pem = client_kp.serialize_pem();
        let p = ClientCertParams {
            device_label: "alice-laptop",
            device_id: "fp-abc123",
            client_public_key_pem: &client_pub_pem,
            validity_days: 90,
        };
        let issued = issue_client_cert(&ca.cert_pem, &ca.key_pkcs8_pem, &p).expect("client cert");
        let meta = parse_cert_metadata(&issued.cert_der).expect("parse");
        assert_eq!(meta.subject_cn.as_deref(), Some("alice-laptop"));
        assert_eq!(meta.san_uri.as_deref(), Some("borg-device:fp-abc123"));
    }
}
