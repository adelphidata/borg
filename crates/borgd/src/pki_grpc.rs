//! gRPC handlers for the `Pki` service.
//!
//! Wired in `daemon.rs` alongside the other services; mTLS is required to
//! reach these RPCs (interceptor enforces). The pairing-code mint here is
//! the admin-side counterpart to the SPAKE2 bootstrap socket — it does not
//! itself run PAKE.

use std::sync::Arc;
use tokio::sync::Mutex;

use cortex::db::{audit_events, AuditChainOutcome, AuditWriter, Database};
use identity::{
    ca::{issue_client_cert, ClientCertParams},
    fingerprint_sha256,
};
use protocols::pki::{
    pki_server::Pki, CertLifecycleResponse, PairedDevice as PbPairedDevice, PairedDeviceList,
    PkiEmpty, RenewCertRequest, RenewCertResponse, RevokeDeviceRequest, StartPairingRequest,
    StartPairingResponse, VerifyAuditResponse,
};
use serde_json::json;
use tonic::{Request, Response, Status};

use crate::auth::Principal;
use crate::pairing::device::mint_pairing_code;

/// Concrete service. Holds the shared writer + CA material.
pub struct PkiSvc {
    db: Arc<Mutex<Database>>,
    audit: AuditWriter,
    ca_cert_pem: String,
    ca_key_pem: String,
}

impl PkiSvc {
    /// Build the PKI gRPC service from the shared DB handle, audit writer, and CA material.
    pub fn new(
        db: Arc<Mutex<Database>>,
        audit: AuditWriter,
        ca_cert_pem: String,
        ca_key_pem: String,
    ) -> Self {
        Self {
            db,
            audit,
            ca_cert_pem,
            ca_key_pem,
        }
    }
}

const PAIRING_TTL_SECS: i64 = 300;
// `pub(crate)` so the `pairing::device` test module can mint codes against
// the same intent the server uses; otherwise tests get "inaccessible" errors.
pub(crate) const PAIRING_INTENT: &str = "device";

#[tonic::async_trait]
impl Pki for PkiSvc {
    async fn start_pairing(
        &self,
        req: Request<StartPairingRequest>,
    ) -> Result<Response<StartPairingResponse>, Status> {
        let principal = req
            .extensions()
            .get::<Principal>()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "unauthenticated".to_string());
        let r = req.into_inner();
        let code = mint_pairing_code();
        let now = chrono::Utc::now().timestamp();
        let expires_at = now + PAIRING_TTL_SECS;

        {
            let g = self.db.lock().await;
            g.create_pairing_code(&code, PAIRING_INTENT, PAIRING_TTL_SECS)
                .await
                .map_err(|e| Status::internal(format!("create pairing code: {e}")))?;
        }
        self.audit
            .emit(
                audit_events::PAIRING_REQUESTED,
                &principal,
                &json!({"device_label": r.device_label, "expires_at": expires_at}),
            )
            .await;

        // Caution: code is intentionally returned to the admin caller. We do
        // NOT log it.
        Ok(Response::new(StartPairingResponse { code, expires_at }))
    }

    async fn renew_cert(
        &self,
        req: Request<RenewCertRequest>,
    ) -> Result<Response<RenewCertResponse>, Status> {
        let principal = req
            .extensions()
            .get::<Principal>()
            .ok_or_else(|| Status::unauthenticated("no principal"))?
            .clone();
        let r = req.into_inner();
        if r.fingerprint != principal.fingerprint {
            return Err(Status::permission_denied(
                "fingerprint mismatch — clients can only renew their own cert",
            ));
        }

        // Look up the device + its current cert.
        let (device_label, _old_serial) = {
            let g = self.db.lock().await;
            let dev = g
                .find_active_paired_device(&principal.fingerprint)
                .await
                .map_err(|e| Status::internal(format!("db: {e}")))?
                .ok_or_else(|| Status::not_found("device not paired"))?;
            (dev.device_label, dev.cert_serial)
        };

        // Mint replacement cert. Reuse a freshly-generated keypair on the
        // server side; the client should stash this and rotate.
        let new_kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| Status::internal(format!("keypair: {e}")))?;
        let new_pub_pem = new_kp.serialize_pem();
        let issued = issue_client_cert(
            &self.ca_cert_pem,
            &self.ca_key_pem,
            &ClientCertParams {
                device_label: &device_label,
                device_id: &principal.fingerprint,
                client_public_key_pem: &new_pub_pem,
                validity_days: 90,
            },
        )
        .map_err(|e| Status::internal(format!("issue cert: {e}")))?;
        let new_fp = fingerprint_sha256(&issued.cert_der);

        let new_serial = {
            let g = self.db.lock().await;
            let serial = g
                .insert_internal_cert(
                    "client",
                    &issued.cert_pem,
                    &new_fp,
                    &device_label,
                    issued.not_before,
                    issued.not_after,
                )
                .await
                .map_err(|e| Status::internal(format!("persist new cert: {e}")))?;
            // The fingerprint-pinning model: on renewal the new cert has a
            // different fingerprint, but we keep the SAME paired_devices row
            // by updating its cert_serial. The client uses the new key+cert.
            g.insert_paired_device(&new_fp, &device_label, "{}", &principal.id, serial)
                .await
                .map_err(|e| Status::internal(format!("update paired device: {e}")))?;
            // Best-effort revoke of the old fingerprint; the new cert is already
            // paired above, so a failure here doesn't compromise the renewal.
            let _ = g
                .add_revoked_fingerprint(&principal.fingerprint, Some("renewed"))
                .await;
            serial
        };
        self.audit
            .emit(
                audit_events::CERT_RENEWED,
                &principal.id,
                &json!({
                    "old_fingerprint": principal.fingerprint,
                    "new_fingerprint": new_fp,
                    "new_serial": new_serial,
                }),
            )
            .await;

        Ok(Response::new(RenewCertResponse {
            new_cert_pem: issued.cert_pem,
            new_key_pkcs8_pem: new_pub_pem,
            not_after: issued.not_after,
        }))
    }

    async fn revoke_device(
        &self,
        req: Request<RevokeDeviceRequest>,
    ) -> Result<Response<PkiEmpty>, Status> {
        let principal = req
            .extensions()
            .get::<Principal>()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "system".to_string());
        let r = req.into_inner();
        let reason = if r.reason.is_empty() {
            None
        } else {
            Some(r.reason.as_str())
        };
        {
            let g = self.db.lock().await;
            g.revoke_paired_device(&r.fingerprint, reason)
                .await
                .map_err(|e| Status::internal(format!("revoke: {e}")))?;
        }
        self.audit
            .emit(
                audit_events::CERT_REVOKED,
                &principal,
                &json!({"fingerprint": r.fingerprint, "reason": r.reason}),
            )
            .await;
        Ok(Response::new(PkiEmpty {}))
    }

    async fn list_paired_devices(
        &self,
        _req: Request<PkiEmpty>,
    ) -> Result<Response<PairedDeviceList>, Status> {
        let rows = {
            let g = self.db.lock().await;
            g.list_paired_devices()
                .await
                .map_err(|e| Status::internal(format!("list: {e}")))?
        };
        let devices = rows
            .into_iter()
            .map(|r| PbPairedDevice {
                id: r.id,
                fingerprint: r.fingerprint,
                device_label: r.device_label,
                paired_at: r.paired_at,
                last_seen_at: r.last_seen_at.unwrap_or(0),
                status: r.status,
                principal_id: r.principal_id,
            })
            .collect();
        Ok(Response::new(PairedDeviceList { devices }))
    }

    async fn get_cert_lifecycle(
        &self,
        req: Request<PkiEmpty>,
    ) -> Result<Response<CertLifecycleResponse>, Status> {
        let principal = req
            .extensions()
            .get::<Principal>()
            .ok_or_else(|| Status::unauthenticated("no principal"))?
            .clone();
        let now = chrono::Utc::now().timestamp();
        let not_after = {
            let g = self.db.lock().await;
            g.paired_device_cert_expiry(&principal.fingerprint)
                .await
                .map_err(|e| Status::internal(format!("db: {e}")))?
                .unwrap_or(0)
        };
        let renewal_due = if not_after == 0 {
            false
        } else {
            let remaining = not_after.saturating_sub(now);
            (0..=crate::auth::verifier::RENEWAL_WINDOW_SECS).contains(&remaining)
        };
        Ok(Response::new(CertLifecycleResponse {
            not_after,
            renewal_due,
        }))
    }

    async fn verify_audit_chain(
        &self,
        _req: Request<PkiEmpty>,
    ) -> Result<Response<VerifyAuditResponse>, Status> {
        let outcome = self.audit.verify().await;
        let resp = match outcome {
            AuditChainOutcome::Ok => VerifyAuditResponse {
                outcome: "ok".into(),
                tampered_seq: -1,
                tombstone_count: 0,
            },
            AuditChainOutcome::OkWithTombstones { count } => VerifyAuditResponse {
                outcome: "ok_with_tombstones".into(),
                tampered_seq: -1,
                tombstone_count: count as i64,
            },
            AuditChainOutcome::Tampered { seq } => VerifyAuditResponse {
                outcome: "tampered".into(),
                tampered_seq: seq,
                tombstone_count: 0,
            },
        };
        Ok(Response::new(resp))
    }
}
