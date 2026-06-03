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

    /// Issue a fresh CA + loopback server leaf for test material.
    fn valid_material() -> ServerCertMaterial {
        let root = ca::generate_root_ca("borg-test-ca").expect("ca");
        let dns = vec![crate::auth::san::LOOPBACK_SAN.to_string()];
        let p = ServerCertParams {
            common_name: "borgd",
            san_dns: &dns,
            validity_days: 90,
        };
        let (server, key_pem) =
            ca::issue_server_cert(&root.cert_pem, &root.key_pkcs8_pem, &p).expect("server");
        ServerCertMaterial {
            server_cert_pem: server.cert_pem,
            server_key_pem: key_pem,
            ca_cert_pem: root.cert_pem,
        }
    }

    /// Install the `ring` crypto provider for this process so that building a
    /// real `tonic` TLS acceptor (which parses the PEMs) doesn't trip the
    /// "ambiguous CryptoProvider" panic — both `ring` and `aws-lc-rs` are in
    /// the dependency tree, so the default can't be auto-detected. Idempotent.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn valid_material_yields_a_config_that_a_tonic_server_can_accept() {
        // `.is_ok()` alone is fluff here: `ServerTlsConfig::new()` is infallible
        // and defers all PEM parsing, so a swapped cert/key or malformed PEM
        // would still produce Ok from `build_server_tls_config`. Feeding the
        // config into `tonic`'s `tls_config` forces it to build a real TLS
        // acceptor, which eagerly parses the Identity + CA roots. That proves
        // the three PEMs actually round-trip into a usable mTLS server config
        // — the property `daemon.rs` relies on.
        ensure_crypto_provider();
        let cfg = build_server_tls_config(&valid_material()).expect("build config");
        tonic::transport::Server::builder()
            .tls_config(cfg)
            .expect("tonic must accept the cert/key/CA material as a valid mTLS acceptor");
    }

    #[test]
    fn swapped_cert_and_key_is_rejected_when_the_acceptor_is_built() {
        // Guards against an operator/refactor mix-up that puts the key in the
        // cert slot (and vice versa). `build_server_tls_config` can't catch it
        // (it just wraps the bytes), but the acceptor build must.
        ensure_crypto_provider();
        let m = valid_material();
        let swapped = ServerCertMaterial {
            server_cert_pem: m.server_key_pem,
            server_key_pem: m.server_cert_pem,
            ca_cert_pem: m.ca_cert_pem,
        };
        let cfg = build_server_tls_config(&swapped).expect("wrapping is infallible");
        assert!(
            tonic::transport::Server::builder().tls_config(cfg).is_err(),
            "a cert/key swap must fail when the TLS acceptor is constructed"
        );
    }

    #[test]
    fn empty_required_pem_fields_are_rejected_eagerly() {
        // Both the cert and the key are required; an empty value for either
        // must error from `build_server_tls_config` itself (before any
        // acceptor build), with a message naming the offending field so the
        // failure is diagnosable. The CA may legitimately be supplied later.
        let base = valid_material();
        let cases: &[(&str, ServerCertMaterial)] = &[
            (
                "server_cert_pem",
                ServerCertMaterial {
                    server_cert_pem: String::new(),
                    ..base.clone()
                },
            ),
            (
                "server_key_pem",
                ServerCertMaterial {
                    server_key_pem: String::new(),
                    ..base.clone()
                },
            ),
        ];
        for (field, material) in cases {
            let err = build_server_tls_config(material)
                .expect_err(&format!("empty {field} must be rejected"));
            assert!(
                err.to_string().contains(field),
                "error for empty {field} must name the field; got: {err}"
            );
        }
    }
}
