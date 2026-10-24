//! Daemon startup CA bootstrap.
//!
//! On first start: mint root CA + server cert, persist cert PEMs in
//! `internal_ca_certs`, stash both private keys in the OS keychain. On
//! subsequent starts: load existing cert from DB and key from keychain.
//! Triggers silent renewal when the server cert is ≤14d from expiry.

use anyhow::{anyhow, Context, Result};
use cortex::db::Database;
use identity::{
    ca::{generate_root_ca, issue_server_cert, ServerCertParams},
    cert::{expires_within, key_matches_cert, parse_cert_metadata, CertMetadata},
    error::PkiError,
    fingerprint_sha256, KekStore,
};

/// Composite material the daemon hands to the gRPC TLS layer.
#[derive(Debug, Clone)]
pub struct CaMaterial {
    /// Root CA certificate (PEM).
    pub ca_cert_pem: String,
    /// Root CA private key (PKCS#8 PEM).
    pub ca_key_pkcs8_pem: String,
    /// Server leaf certificate (PEM).
    pub server_cert_pem: String,
    /// Server leaf private key (PKCS#8 PEM).
    pub server_key_pkcs8_pem: String,
    /// Serial number of the server cert in `internal_ca_certs`.
    pub server_cert_serial: i64,
}

/// Keychain account names.
const ROOT_CA_KEY: &str = "root-ca-key";
const SERVER_CERT_KEY: &str = "server-cert-key";

/// Days before expiry at which the server cert auto-renews.
const RENEWAL_WINDOW_DAYS: u32 = 14;

/// Default validity in days for the server cert.
const SERVER_VALIDITY_DAYS: u32 = 90;

/// Ensure a root CA exists in DB + keychain. Mints one if absent.
///
/// Self-heals when the DB has a CA cert but the KEK has no matching private
/// key — happens after a KEK backend swap (e.g. lost passphrase, OS keychain
/// → file fallback, or a non-atomic bootstrap that persisted the cert before
/// the key write succeeded), or when another daemon on the same keychain
/// overwrote the key with its own. The orphaned cert is revoked and a fresh
/// CA is minted so the daemon boots instead of crash-looping or signing
/// certs that do not chain.
pub async fn ensure_root_ca(db: &Database, kek: &dyn KekStore) -> Result<(String, String)> {
    if let Some(row) = db
        .latest_active_cert("root")
        .await
        .context("query root ca")?
    {
        let reason = match kek.get(ROOT_CA_KEY) {
            Ok(key_bytes) => {
                let key_pem = String::from_utf8(key_bytes).context("root CA key not utf-8 PEM")?;
                if key_matches_cert(&key_pem, &row.pem).context("check root CA key")? {
                    return Ok((row.pem, key_pem));
                }
                "kek-key-mismatch-self-heal"
            }
            Err(PkiError::NotFound(_)) => "kek-key-missing-self-heal",
            Err(e) => return Err(anyhow!("load root CA key from keychain: {e}")),
        };
        tracing::warn!(
            serial = row.serial,
            fingerprint = %row.fingerprint,
            reason,
            "root CA cert in DB but its private key is missing from the KEK or does not match; revoking and minting fresh CA — paired devices will need to re-pair"
        );
        db.mark_cert_revoked(row.serial, Some(reason))
            .await
            .context("revoke orphaned root CA")?;
        // Server-cert revocation is handled by the unconditional sweep below
        // — running it on every fresh-root mint also covers crash recovery
        // (process died between the two revoke calls).
    }

    let root = generate_root_ca("borgd-root").context("mint root CA")?;
    let fp = fingerprint_sha256(&root.cert_der);
    let meta = parse_cert_metadata(&root.cert_der).context("parse minted root CA")?;
    kek.put(ROOT_CA_KEY, root.key_pkcs8_pem.as_bytes())
        .map_err(|e| anyhow!("stash root CA key in keychain: {e}"))?;
    db.insert_internal_cert(
        "root",
        &root.cert_pem,
        &fp,
        meta.subject_cn.as_deref().unwrap_or("borgd-root"),
        meta.not_before,
        meta.not_after,
    )
    .await
    .context("persist root CA")?;

    // Any active server cert here was issued by the previous root and is now
    // chained to a revoked CA. Sweep it so the next `ensure_server_cert`
    // mints a fresh leaf under the new root. Idempotent: a no-op on first
    // boot when there are no server rows yet.
    if let Some(srv) = db
        .latest_active_cert("server")
        .await
        .context("query server cert after root rotation")?
    {
        db.mark_cert_revoked(srv.serial, Some("root-ca-rotated-self-heal"))
            .await
            .context("revoke server cert chained to rotated CA")?;
        if let Err(e) = kek.delete(SERVER_CERT_KEY) {
            tracing::warn!(error = %e, "drop orphaned server KEK key after root rotation");
        }
    }

    Ok((root.cert_pem, root.key_pkcs8_pem))
}

/// Ensure an active server cert exists. Renews if ≤14d to expiry.
pub async fn ensure_server_cert(
    db: &Database,
    kek: &dyn KekStore,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    san_dns: &[String],
) -> Result<(String, String, i64)> {
    let now = chrono::Utc::now().timestamp();

    if let Some(row) = db
        .latest_active_cert("server")
        .await
        .context("query server cert")?
    {
        let reason = match kek.get(SERVER_CERT_KEY) {
            Ok(key_bytes) => {
                let key_pem = String::from_utf8(key_bytes).context("server key not utf-8 PEM")?;
                if !key_matches_cert(&key_pem, &row.pem).context("check server cert key")? {
                    "kek-key-mismatch-self-heal"
                } else {
                    let meta = CertMetadata {
                        subject_cn: None,
                        san_uri: None,
                        not_before: row.not_before,
                        not_after: row.not_after,
                        fingerprint: row.fingerprint.clone(),
                    };
                    if !expires_within(&meta, now, RENEWAL_WINDOW_DAYS) {
                        return Ok((row.pem, key_pem, row.serial));
                    }
                    // Renew: mint new, mark old renewed.
                    let (issued, new_key) = mint_server_cert(ca_cert_pem, ca_key_pem, san_dns)?;
                    let serial = persist_server_cert(db, kek, &issued, &new_key).await?;
                    db.mark_cert_renewed(row.serial)
                        .await
                        .context("mark old renewed")?;
                    return Ok((issued.cert_pem, new_key, serial));
                }
            }
            Err(PkiError::NotFound(_)) => "kek-key-missing-self-heal",
            Err(e) => return Err(anyhow!("load server cert key from keychain: {e}")),
        };
        tracing::warn!(
            serial = row.serial,
            reason,
            "server cert in DB but its private key is missing from the KEK or does not match; revoking and re-issuing"
        );
        db.mark_cert_revoked(row.serial, Some(reason))
            .await
            .context("revoke orphaned server cert")?;
    }

    let (issued, new_key) = mint_server_cert(ca_cert_pem, ca_key_pem, san_dns)?;
    let serial = persist_server_cert(db, kek, &issued, &new_key).await?;
    Ok((issued.cert_pem, new_key, serial))
}

fn mint_server_cert(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    san_dns: &[String],
) -> Result<(identity::IssuedCert, String)> {
    let params = ServerCertParams {
        common_name: "borgd",
        san_dns,
        validity_days: SERVER_VALIDITY_DAYS,
    };
    issue_server_cert(ca_cert_pem, ca_key_pem, &params).context("issue server cert")
}

async fn persist_server_cert(
    db: &Database,
    kek: &dyn KekStore,
    issued: &identity::IssuedCert,
    key_pem: &str,
) -> Result<i64> {
    let fp = fingerprint_sha256(&issued.cert_der);
    // KEK first so a mid-bootstrap crash leaves an orphan key (harmless) and
    // no DB row, instead of an orphan cert that triggers self-heal next boot.
    kek.put(SERVER_CERT_KEY, key_pem.as_bytes())
        .map_err(|e| anyhow!("stash server key: {e}"))?;
    let serial = db
        .insert_internal_cert(
            "server",
            &issued.cert_pem,
            &fp,
            "borgd",
            issued.not_before,
            issued.not_after,
        )
        .await?;
    Ok(serial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::MemoryKekStore;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn root_ca_minted_once_then_loaded_from_db() {
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (pem1, key1) = ensure_root_ca(&d, &kek).await.expect("first");
        let (pem2, key2) = ensure_root_ca(&d, &kek).await.expect("second");
        // Identical CA on second call (loaded, not regenerated).
        assert_eq!(pem1, pem2);
        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn server_cert_inside_the_renewal_window_is_reissued() {
        // The renewal branch is what the daily check in `daemon::run` relies
        // on. It was previously reachable only at daemon start, so a daemon up
        // longer than SERVER_VALIDITY_DAYS - RENEWAL_WINDOW_DAYS (~76 days)
        // never renewed and every mTLS handshake failed on day 90. Pin the
        // renewal itself so the periodic caller has something real to call.
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        let san = vec![crate::auth::san::LOOPBACK_SAN.to_string()];

        let (_, _, serial1) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("initial issue");

        // Backdate expiry to inside the renewal window.
        let soon = chrono::Utc::now().timestamp() + (RENEWAL_WINDOW_DAYS as i64 - 1) * 86_400;
        d.conn()
            .execute(
                "UPDATE internal_ca_certs SET not_after = ?1 WHERE serial = ?2",
                libsql::params![soon, serial1],
            )
            .await
            .expect("backdate expiry");

        let (_, _, serial2) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("renewal");

        assert_ne!(
            serial1, serial2,
            "a cert inside the renewal window must be reissued"
        );

        // And the fresh one is now the active row, so a restart serves it.
        let active = d
            .latest_active_cert("server")
            .await
            .expect("query")
            .expect("an active server cert must exist");
        assert_eq!(active.serial, serial2);
    }

    #[tokio::test]
    async fn server_cert_minted_once_then_loaded_from_db() {
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        let san = vec![crate::auth::san::LOOPBACK_SAN.to_string()];
        let (s1, _, serial1) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("first");
        let (s2, _, serial2) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("second");
        assert_eq!(s1, s2);
        assert_eq!(serial1, serial2);
    }

    #[tokio::test]
    async fn missing_keychain_entry_self_heals_with_fresh_ca() {
        // Self-healing on KEK loss is what unblocks the daemon after a backend
        // swap (e.g. losing kek.enc/passphrase on a headless VPS). Without it
        // the orphaned cert in `internal_ca_certs` puts borgd in a crash loop.
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (orig_pem, orig_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        // Simulate KEK loss (e.g. file backend wiped, OS keychain swapped).
        kek.delete(ROOT_CA_KEY).expect("delete");

        let (new_pem, new_key) = ensure_root_ca(&d, &kek)
            .await
            .expect("self-heal mints fresh CA");
        assert_ne!(orig_pem, new_pem, "must mint a brand new CA");
        assert_ne!(orig_key, new_key);

        // The orphaned row must be revoked, not still active.
        let active = d
            .latest_active_cert("root")
            .await
            .expect("query")
            .expect("row");
        assert_ne!(
            active.pem, orig_pem,
            "old cert should no longer be the latest active"
        );
        assert_eq!(active.pem, new_pem);
    }

    #[tokio::test]
    async fn root_self_heal_also_revokes_server_cert() {
        // If only the root key is missing but the server key is intact, the
        // pre-fix code would mint a new CA, then `ensure_server_cert` would
        // see a still-active server row + valid key and reuse it — yielding
        // a leaf chained to a CA that no longer exists. The cascade in
        // `ensure_root_ca` must revoke the server row too.
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        let san = vec![crate::auth::san::LOOPBACK_SAN.to_string()];
        let (orig_server, _, orig_serial) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("server");

        // Lose only the root CA key.
        kek.delete(ROOT_CA_KEY).expect("wipe root key");

        let (new_ca_pem, new_ca_key) = ensure_root_ca(&d, &kek).await.expect("self-heal ca");
        assert_ne!(ca_pem, new_ca_pem);

        let (new_server, _, new_serial) =
            ensure_server_cert(&d, &kek, &new_ca_pem, &new_ca_key, &san)
                .await
                .expect("server re-issue");
        assert_ne!(orig_server, new_server, "server cert must be re-issued");
        assert_ne!(orig_serial, new_serial);
    }

    #[tokio::test]
    async fn server_cert_self_heals_when_key_missing() {
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        let san = vec![crate::auth::san::LOOPBACK_SAN.to_string()];
        let (orig, _, orig_serial) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("first");
        kek.delete(SERVER_CERT_KEY).expect("wipe server key");

        let (fresh, _, fresh_serial) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("self-heal");
        assert_ne!(orig, fresh);
        assert_ne!(orig_serial, fresh_serial);
    }

    #[tokio::test]
    async fn root_ca_self_heals_when_another_daemon_overwrote_its_key() {
        // Two daemons on one OS keychain share the account name. The second
        // one's key is a valid PEM, so only a key/cert check catches it.
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (orig_pem, _) = ensure_root_ca(&d, &kek).await.expect("ca");
        let foreign = generate_root_ca("borgd-root").expect("foreign ca");
        kek.put(ROOT_CA_KEY, foreign.key_pkcs8_pem.as_bytes())
            .expect("overwrite");

        let (new_pem, new_key) = ensure_root_ca(&d, &kek).await.expect("self-heal");
        assert_ne!(orig_pem, new_pem, "must mint a brand new CA");
        assert!(key_matches_cert(&new_key, &new_pem).expect("check"));
        let active = d
            .latest_active_cert("root")
            .await
            .expect("query")
            .expect("row");
        assert_eq!(active.pem, new_pem);
    }

    #[tokio::test]
    async fn server_cert_self_heals_when_another_daemon_overwrote_its_key() {
        let kek = MemoryKekStore::new();
        let d = db().await;
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).await.expect("ca");
        let san = vec![crate::auth::san::LOOPBACK_SAN.to_string()];
        let (orig, _, orig_serial) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("first");
        let (_, foreign_key) = mint_server_cert(&ca_pem, &ca_key, &san).expect("foreign");
        kek.put(SERVER_CERT_KEY, foreign_key.as_bytes())
            .expect("overwrite");

        let (fresh, fresh_key, fresh_serial) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san)
            .await
            .expect("self-heal");
        assert_ne!(orig, fresh);
        assert_ne!(orig_serial, fresh_serial);
        assert!(key_matches_cert(&fresh_key, &fresh).expect("check"));
    }
}
