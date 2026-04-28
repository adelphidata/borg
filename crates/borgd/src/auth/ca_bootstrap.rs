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
    cert::{expires_within, parse_cert_metadata, CertMetadata},
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
pub fn ensure_root_ca(db: &Database, kek: &dyn KekStore) -> Result<(String, String)> {
    if let Some(row) = db.latest_active_cert("root").context("query root ca")? {
        let key_bytes = kek
            .get(ROOT_CA_KEY)
            .map_err(|e| anyhow!("load root CA key from keychain: {e}"))?;
        let key_pem = String::from_utf8(key_bytes).context("root CA key not utf-8 PEM")?;
        return Ok((row.pem, key_pem));
    }

    let root = generate_root_ca("borgd-root").context("mint root CA")?;
    let fp = fingerprint_sha256(&root.cert_der);
    let meta = parse_cert_metadata(&root.cert_der).context("parse minted root CA")?;
    db.insert_internal_cert(
        "root",
        &root.cert_pem,
        &fp,
        meta.subject_cn.as_deref().unwrap_or("borgd-root"),
        meta.not_before,
        meta.not_after,
    )
    .context("persist root CA")?;
    kek.put(ROOT_CA_KEY, root.key_pkcs8_pem.as_bytes())
        .map_err(|e| anyhow!("stash root CA key in keychain: {e}"))?;

    Ok((root.cert_pem, root.key_pkcs8_pem))
}

/// Ensure an active server cert exists. Renews if ≤14d to expiry.
pub fn ensure_server_cert(
    db: &Database,
    kek: &dyn KekStore,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    san_dns: &[String],
) -> Result<(String, String, i64)> {
    let now = chrono::Utc::now().timestamp();

    if let Some(row) = db
        .latest_active_cert("server")
        .context("query server cert")?
    {
        let key_bytes = kek
            .get(SERVER_CERT_KEY)
            .map_err(|e| anyhow!("load server cert key from keychain: {e}"))?;
        let key_pem = String::from_utf8(key_bytes).context("server key not utf-8 PEM")?;
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
        let serial = persist_server_cert(db, kek, &issued, &new_key)?;
        db.mark_cert_renewed(row.serial)
            .context("mark old renewed")?;
        return Ok((issued.cert_pem, new_key, serial));
    }

    let (issued, new_key) = mint_server_cert(ca_cert_pem, ca_key_pem, san_dns)?;
    let serial = persist_server_cert(db, kek, &issued, &new_key)?;
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

fn persist_server_cert(
    db: &Database,
    kek: &dyn KekStore,
    issued: &identity::IssuedCert,
    key_pem: &str,
) -> Result<i64> {
    let fp = fingerprint_sha256(&issued.cert_der);
    let serial = db.insert_internal_cert(
        "server",
        &issued.cert_pem,
        &fp,
        "borgd",
        issued.not_before,
        issued.not_after,
    )?;
    kek.put(SERVER_CERT_KEY, key_pem.as_bytes())
        .map_err(|e| anyhow!("stash server key: {e}"))?;
    Ok(serial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::MemoryKekStore;
    use rusqlite::Connection;

    fn db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("db")).expect("init")
    }

    #[test]
    fn root_ca_minted_once_then_loaded_from_db() {
        let kek = MemoryKekStore::new();
        let d = db();
        let (pem1, key1) = ensure_root_ca(&d, &kek).expect("first");
        let (pem2, key2) = ensure_root_ca(&d, &kek).expect("second");
        // Identical CA on second call (loaded, not regenerated).
        assert_eq!(pem1, pem2);
        assert_eq!(key1, key2);
    }

    #[test]
    fn server_cert_minted_once_then_loaded_from_db() {
        let kek = MemoryKekStore::new();
        let d = db();
        let (ca_pem, ca_key) = ensure_root_ca(&d, &kek).expect("ca");
        let san = vec!["localhost".to_string()];
        let (s1, _, serial1) = ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san).expect("first");
        let (s2, _, serial2) =
            ensure_server_cert(&d, &kek, &ca_pem, &ca_key, &san).expect("second");
        assert_eq!(s1, s2);
        assert_eq!(serial1, serial2);
    }

    #[test]
    fn missing_keychain_entry_fails_loud_when_db_has_cert() {
        let kek = MemoryKekStore::new();
        let d = db();
        ensure_root_ca(&d, &kek).expect("ca");
        // Wipe the kek entry — simulates corrupt keychain.
        kek.delete(ROOT_CA_KEY).expect("delete");
        let res = ensure_root_ca(&d, &kek);
        assert!(
            res.is_err(),
            "ensure_root_ca should fail when keychain entry missing"
        );
    }
}
