//! Build a `tonic::transport::ServerTlsConfig` from the daemon's CA + server cert.

use anyhow::{anyhow, Result};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// Server-side TLS material needed to build the daemon's gRPC `ServerTlsConfig`.
#[derive(Debug, Clone)]
pub struct ServerCertMaterial {
    /// Server leaf certificate (PEM).
    pub server_cert_pem: String,
    /// Server leaf private key (PEM).
    pub server_key_pem: String,
    /// Root CA certificate trusted for client mTLS (PEM).
    pub ca_cert_pem: String,
}

/// Build an mTLS-required `ServerTlsConfig`. Clients without a CA-signed cert
/// fail the handshake before the auth interceptor runs.
pub fn build_server_tls_config(material: &ServerCertMaterial) -> Result<ServerTlsConfig> {
    if material.server_cert_pem.is_empty() {
        return Err(anyhow!("server_cert_pem is empty"));
    }
    if material.server_key_pem.is_empty() {
        return Err(anyhow!("server_key_pem is empty"));
    }
    let identity = Identity::from_pem(
        material.server_cert_pem.as_bytes(),
        material.server_key_pem.as_bytes(),
    );
    let ca = Certificate::from_pem(material.ca_cert_pem.as_bytes());
    Ok(ServerTlsConfig::new().identity(identity).client_ca_root(ca))
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::{ca, ServerCertParams};

    fn fresh_material() -> ServerCertMaterial {
        let root = ca::generate_root_ca("borg-test-ca").expect("ca");
        let dns = vec!["localhost".to_string()];
        let (server, key_pem) = ca::issue_server_cert(
            &root.cert_pem,
            &root.key_pkcs8_pem,
            &ServerCertParams {
                common_name: "borgd",
                san_dns: &dns,
                validity_days: 90,
            },
        )
        .expect("server");
        ServerCertMaterial {
            server_cert_pem: server.cert_pem,
            server_key_pem: key_pem,
            ca_cert_pem: root.cert_pem,
        }
    }

    /// The build accepts a freshly-issued cert+key+CA triple, and the resulting
    /// config is usable as a `tonic::Server` TLS configuration. This catches
    /// regressions where `Identity::from_pem` later panics on the bytes we
    /// constructed (e.g. format drift in the `identity` crate).
    #[test]
    fn build_accepts_fresh_material_and_config_is_usable() {
        let cfg = build_server_tls_config(&fresh_material()).expect("build_server_tls_config");
        // Pass through `tonic::transport::Server` to prove the config is wired
        // correctly — `tls_config` returns Err on malformed configs.
        tonic::transport::Server::builder()
            .tls_config(cfg)
            .expect("tls_config must accept the produced ServerTlsConfig");
    }

    /// Empty server cert and empty server key must each produce an explicit
    /// error from our pre-flight checks rather than relying on tonic to fail
    /// later. The error message must name the missing field so operators can
    /// triage from the log line.
    struct MissingMaterialCase {
        label: &'static str,
        mutate: fn(&mut ServerCertMaterial),
        expected_substr: &'static str,
    }

    #[test]
    fn build_rejects_missing_server_material() {
        let cases = [
            MissingMaterialCase {
                label: "empty server_cert_pem",
                mutate: |m| m.server_cert_pem.clear(),
                expected_substr: "server_cert_pem",
            },
            MissingMaterialCase {
                label: "empty server_key_pem",
                mutate: |m| m.server_key_pem.clear(),
                expected_substr: "server_key_pem",
            },
        ];
        for case in cases {
            let mut material = fresh_material();
            (case.mutate)(&mut material);
            let err = build_server_tls_config(&material)
                .err()
                .unwrap_or_else(|| panic!("{}: expected error, got Ok", case.label));
            let msg = err.to_string();
            assert!(
                msg.contains(case.expected_substr),
                "{}: error {msg:?} did not mention {:?}",
                case.label,
                case.expected_substr,
            );
        }
    }
}
