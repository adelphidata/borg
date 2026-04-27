//! Build a `tonic::transport::ServerTlsConfig` from the daemon's CA + server cert.

use anyhow::{anyhow, Result};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

#[derive(Debug, Clone)]
pub struct ServerCertMaterial {
    pub server_cert_pem: String,
    pub server_key_pem: String,
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
    Ok(ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(ca))
}

#[cfg(test)]
mod tests {
    use super::*;
    use borg_pki::{ca, ServerCertParams};

    #[test]
    fn build_succeeds_with_valid_material() {
        let root = ca::generate_root_ca("borg-test-ca").expect("ca");
        let dns = vec!["localhost".to_string()];
        let p = ServerCertParams {
            common_name: "borgd",
            san_dns: &dns,
            validity_days: 90,
        };
        let (server, key_pem) =
            ca::issue_server_cert(&root.cert_pem, &root.key_pkcs8_pem, &p).expect("server");
        let cfg = build_server_tls_config(&ServerCertMaterial {
            server_cert_pem: server.cert_pem,
            server_key_pem: key_pem,
            ca_cert_pem: root.cert_pem,
        });
        assert!(cfg.is_ok());
    }

    #[test]
    fn build_rejects_empty_server_cert() {
        let root = ca::generate_root_ca("borg-test-ca").expect("ca");
        let cfg = build_server_tls_config(&ServerCertMaterial {
            server_cert_pem: String::new(),
            server_key_pem: "x".into(),
            ca_cert_pem: root.cert_pem,
        });
        assert!(cfg.is_err());
    }
}
