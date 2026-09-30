//! Tonic interceptor: extract peer cert → run verifier → stamp Principal.

use std::sync::Arc;

use cortex::db::AuditWriter;
use identity::cert::fingerprint_sha256;
use serde_json::json;
use tonic::transport::server::TlsConnectInfo;
use tonic::{Request, Status};

use super::verifier::{PeerCtx, Verifier, VerifierError};

/// Authenticated peer identity attached to each gRPC request.
#[derive(Debug, Clone)]
pub struct Principal {
    /// Stable principal identifier (e.g. paired device label).
    pub id: String,
    /// SHA-256 fingerprint of the peer's leaf certificate (hex).
    pub fingerprint: String,
}

/// Build the tonic interceptor that authenticates peers via mTLS + verifier.
///
/// The large `Err` variant is `tonic::Status`, whose size is fixed by tonic's
/// own interceptor signature — boxing it here would no longer satisfy the
/// trait bound tonic requires.
#[allow(clippy::result_large_err)]
pub fn auth_interceptor(
    verifier: Arc<dyn Verifier>,
    audit: AuditWriter,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone + Send + Sync + 'static {
    move |mut req: Request<()>| {
        // Peer IP only available on TCP; UDS connections leave it None. The
        // Tailscale verifier needs it to enforce node-identity pinning;
        // TOFU ignores it.
        let tcp_info = req
            .extensions()
            .get::<TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>();
        let peer_ip = tcp_info
            .and_then(|tls| tls.get_ref().remote_addr())
            .map(|sa| sa.ip());

        let tcp_certs = tcp_info.and_then(tonic::transport::server::TlsConnectInfo::peer_certs);
        #[cfg(unix)]
        let uds_certs = req
            .extensions()
            .get::<TlsConnectInfo<tonic::transport::server::UdsConnectInfo>>()
            .and_then(tonic::transport::server::TlsConnectInfo::peer_certs);
        #[cfg(not(unix))]
        let uds_certs = None;
        let certs = tcp_certs.or(uds_certs);

        let leaf_der: Vec<u8> = certs
            .as_ref()
            .and_then(|cs| cs.first().map(|c| c.as_ref().to_vec()))
            .ok_or_else(|| {
                // Interceptor closure is sync; audit emit is async post-libsql
                // migration. Schedule the write on the current runtime so the
                // auth-failure row still lands in the hash chain. Failing the
                // request itself stays synchronous.
                let audit = audit.clone();
                tokio::spawn(async move {
                    audit
                        .emit(
                            cortex::db::audit_events::GRPC_AUTH_FAILURE,
                            "system",
                            &json!({"reason": "no_peer_cert"}),
                        )
                        .await;
                });
                Status::unauthenticated("no client certificate presented")
            })?;

        let fingerprint = fingerprint_sha256(&leaf_der);
        let peer_ctx = PeerCtx { peer_ip };

        match verifier.verify_with_peer(&fingerprint, None, peer_ctx) {
            Ok(principal_id) => {
                let principal = Principal {
                    id: principal_id,
                    fingerprint,
                };
                req.extensions_mut().insert(principal);
                Ok(req)
            }
            Err(e) => {
                let audit_clone = audit.clone();
                let reason = e.to_string();
                let fp_failure = fingerprint.clone();
                let fp_tofu = fingerprint.clone();
                let is_tofu = matches!(e, VerifierError::TofuMismatch { .. });
                tokio::spawn(async move {
                    audit_clone
                        .emit(
                            cortex::db::audit_events::GRPC_AUTH_FAILURE,
                            "system",
                            &json!({"reason": reason, "fingerprint": fp_failure}),
                        )
                        .await;
                    if is_tofu {
                        audit_clone
                            .emit(
                                cortex::db::audit_events::TOFU_MISMATCH,
                                "system",
                                &json!({"fingerprint": fp_tofu}),
                            )
                            .await;
                    }
                });
                Err(Status::unauthenticated(e.to_string()))
            }
        }
    }
}
