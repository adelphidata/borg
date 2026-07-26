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

/// Pairing codes are short and human-transcribed, so the window is kept to the
/// length of one supervised pairing attempt to limit brute-force surface.
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
        // Trust model: the Pki service is mounted twice. On the local UDS
        // socket there is NO auth interceptor, so no `Principal` is stamped —
        // that is the host-local admin plane (filesystem-isolated) and may
        // revoke any device (this is how `borg revoke <lost-device>` works).
        // On the mTLS TCP listener every request carries a `Principal`; such a
        // remote, paired device may revoke ONLY its own certificate. Without
        // this, any low-trust paired device could revoke the operator's laptop
        // — a self-lockout DoS (finding M5).
        let principal = req.extensions().get::<Principal>().cloned();
        let r = req.into_inner();
        if let Some(p) = &principal {
            if r.fingerprint != p.fingerprint {
                return Err(Status::permission_denied(
                    "remote callers may only revoke their own device; \
                     revoke other devices from the host with the local `borg` CLI",
                ));
            }
        }
        let actor = principal
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "system".to_string());
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
                &actor,
                &json!({"fingerprint": r.fingerprint, "reason": r.reason}),
            )
            .await;
        Ok(Response::new(PkiEmpty {}))
    }

    async fn list_paired_devices(
        &self,
        req: Request<PkiEmpty>,
    ) -> Result<Response<PairedDeviceList>, Status> {
        // The full device roster (labels + fingerprints) is admin-only. A
        // remote paired device carries a `Principal` (mTLS TCP path); letting it
        // enumerate every other device would hand it the exact fingerprints to
        // target with `revoke_device`. Only the local UDS admin plane (no
        // `Principal`) may list. Finding M5.
        if req.extensions().get::<Principal>().is_some() {
            return Err(Status::permission_denied(
                "listing paired devices is restricted to the host-local admin plane",
            ));
        }
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    async fn svc() -> PkiSvc {
        let db = Arc::new(Mutex::new(Database::test_db().await));
        let audit = AuditWriter::new(db.clone());
        PkiSvc::new(db, audit, "ca-cert".into(), "ca-key".into())
    }

    fn revoke_req(target_fp: &str, principal: Option<Principal>) -> Request<RevokeDeviceRequest> {
        let mut req = Request::new(RevokeDeviceRequest {
            fingerprint: target_fp.into(),
            reason: "test".into(),
        });
        if let Some(p) = principal {
            req.extensions_mut().insert(p);
        }
        req
    }

    fn principal(fp: &str) -> Principal {
        Principal {
            id: format!("dev-{fp}"),
            fingerprint: fp.into(),
        }
    }

    #[tokio::test]
    async fn remote_device_cannot_revoke_another_device() {
        // M5: a remote (mTLS) caller carries a Principal and must not revoke a
        // fingerprint that isn't its own — otherwise any paired device could
        // lock out the operator's laptop.
        let svc = svc().await;
        let err = svc
            .revoke_device(revoke_req("fp-victim", Some(principal("fp-attacker"))))
            .await
            .expect_err("cross-device revoke must be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn remote_device_can_revoke_itself() {
        // Self-revoke (e.g. a device retiring its own compromised key) stays
        // allowed for remote callers.
        let svc = svc().await;
        svc.revoke_device(revoke_req("fp-self", Some(principal("fp-self"))))
            .await
            .expect("self-revoke must succeed");
    }

    #[tokio::test]
    async fn local_admin_plane_can_revoke_any_device() {
        // No Principal ⟺ the host-local UDS admin plane (no interceptor). The
        // operator revoking a lost device via `borg revoke` must still work.
        let svc = svc().await;
        svc.revoke_device(revoke_req("fp-lost-device", None))
            .await
            .expect("local admin revoke of any device must succeed");
    }

    #[tokio::test]
    async fn remote_device_cannot_list_paired_devices() {
        // M5: the device roster (labels + fingerprints) is admin-only; a remote
        // caller must not be able to enumerate targets to revoke.
        let svc = svc().await;
        let mut req = Request::new(PkiEmpty {});
        req.extensions_mut().insert(principal("fp-remote"));
        let err = svc
            .list_paired_devices(req)
            .await
            .expect_err("remote list must be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn local_admin_plane_can_list_paired_devices() {
        let svc = svc().await;
        svc.list_paired_devices(Request::new(PkiEmpty {}))
            .await
            .expect("local admin list must succeed");
    }
}
