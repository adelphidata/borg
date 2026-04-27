//! Tonic interceptor: extract peer cert → run verifier → stamp Principal.

use std::sync::Arc;

use cortex::db::AuditWriter;
use identity::cert::fingerprint_sha256;
use serde_json::json;
use tonic::transport::server::TlsConnectInfo;
use tonic::{Request, Status};

use super::verifier::{Verifier, VerifierError};

#[derive(Debug, Clone)]
pub struct Principal {
    pub id: String,
    pub fingerprint: String,
}

pub fn auth_interceptor(
    verifier: Arc<dyn Verifier>,
    audit: AuditWriter,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone + Send + Sync + 'static {
    move |mut req: Request<()>| {
        let tcp_certs = req
            .extensions()
            .get::<TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
            .and_then(|tls| tls.peer_certs());
        let uds_certs = req
            .extensions()
            .get::<TlsConnectInfo<tonic::transport::server::UdsConnectInfo>>()
            .and_then(|tls| tls.peer_certs());
        let certs = tcp_certs.or(uds_certs);

        let leaf_der: Vec<u8> = certs
            .as_ref()
            .and_then(|cs| cs.first().map(|c| c.as_ref().to_vec()))
            .ok_or_else(|| {
                audit.emit(
                    cortex::db::audit_events::GRPC_AUTH_FAILURE,
                    "system",
                    &json!({"reason": "no_peer_cert"}),
                );
                Status::unauthenticated("no client certificate presented")
            })?;

        let fingerprint = fingerprint_sha256(&leaf_der);

        match verifier.verify(&fingerprint, None) {
            Ok(principal_id) => {
                let principal = Principal {
                    id: principal_id,
                    fingerprint,
                };
                req.extensions_mut().insert(principal);
                Ok(req)
            }
            Err(e) => {
                audit.emit(
                    cortex::db::audit_events::GRPC_AUTH_FAILURE,
                    "system",
                    &json!({"reason": e.to_string(), "fingerprint": fingerprint}),
                );
                if let VerifierError::TofuMismatch { .. } = e {
                    audit.emit(
                        cortex::db::audit_events::TOFU_MISMATCH,
                        "system",
                        &json!({"fingerprint": fingerprint}),
                    );
                }
                Err(Status::unauthenticated(e.to_string()))
            }
        }
    }
}
