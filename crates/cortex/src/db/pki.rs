//! Persistence for the daemon's internal CA, paired devices, and revocation list.
use anyhow::{anyhow, Result};
use libsql::params;

use super::Database;

#[derive(Debug, Clone)]
pub struct InternalCertRow {
    pub serial: i64,
    pub kind: String,
    pub pem: String,
    pub fingerprint: String,
    pub subject: String,
    pub not_before: i64,
    pub not_after: i64,
    pub status: String,
    pub issued_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PairedDeviceRow {
    pub id: i64,
    pub fingerprint: String,
    pub device_label: String,
    pub scope_json: String,
    pub paired_at: i64,
    pub last_seen_at: Option<i64>,
    pub status: String,
    pub principal_id: String,
    pub cert_serial: i64,
}

impl Database {
    pub async fn insert_internal_cert(
        &self,
        kind: &str,
        pem: &str,
        fingerprint: &str,
        subject: &str,
        not_before: i64,
        not_after: i64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO internal_ca_certs
             (kind, pem, fingerprint, subject, not_before, not_after, status, issued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
                params!(kind, pem, fingerprint, subject, not_before, not_after, now),
            )
            .await?;
        Ok(self.conn.last_insert_rowid())
    }

    pub async fn latest_active_cert(&self, kind: &str) -> Result<Option<InternalCertRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT serial, kind, pem, fingerprint, subject, not_before, not_after,
                    status, issued_at, revoked_at
             FROM internal_ca_certs
             WHERE kind = ?1 AND status = 'active'
             ORDER BY issued_at DESC LIMIT 1",
                params!(kind),
            )
            .await?;
        match rows.next().await? {
            Some(r) => Ok(Some(InternalCertRow {
                serial: r.get::<i64>(0)?,
                kind: r.get::<String>(1)?,
                pem: r.get::<String>(2)?,
                fingerprint: r.get::<String>(3)?,
                subject: r.get::<String>(4)?,
                not_before: r.get::<i64>(5)?,
                not_after: r.get::<i64>(6)?,
                status: r.get::<String>(7)?,
                issued_at: r.get::<i64>(8)?,
                revoked_at: r.get::<Option<i64>>(9)?,
            })),
            None => Ok(None),
        }
    }

    pub async fn mark_cert_renewed(&self, serial: i64) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE internal_ca_certs SET status = 'renewed' WHERE serial = ?1",
                params!(serial),
            )
            .await?;
        if n == 0 {
            return Err(anyhow!("no internal cert with serial {serial}"));
        }
        Ok(())
    }

    pub async fn mark_cert_revoked(&self, serial: i64, reason: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE internal_ca_certs
             SET status = 'revoked', revoked_at = ?1
             WHERE serial = ?2 AND status != 'revoked'",
                params!(now, serial),
            )
            .await?;
        let fingerprint: Option<String> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT fingerprint FROM internal_ca_certs WHERE serial = ?1",
                    params!(serial),
                )
                .await?;
            match rows.next().await? {
                Some(row) => Some(row.get::<String>(0)?),
                None => None,
            }
        };
        if let Some(fp) = fingerprint {
            self.add_revoked_fingerprint(&fp, reason).await?;
        }
        Ok(())
    }

    pub async fn insert_paired_device(
        &self,
        fingerprint: &str,
        device_label: &str,
        scope_json: &str,
        principal_id: &str,
        cert_serial: i64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO paired_devices
             (fingerprint, device_label, scope_json, paired_at, status, principal_id, cert_serial)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6)
             ON CONFLICT(fingerprint) DO UPDATE SET
                device_label = excluded.device_label,
                scope_json   = excluded.scope_json,
                principal_id = excluded.principal_id,
                cert_serial  = excluded.cert_serial,
                status       = 'active'",
                params!(
                    fingerprint,
                    device_label,
                    scope_json,
                    now,
                    principal_id,
                    cert_serial
                ),
            )
            .await?;
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM paired_devices WHERE fingerprint = ?1",
                params!(fingerprint),
            )
            .await?;
        let id: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => anyhow::bail!("paired_devices row vanished after upsert"),
        };
        Ok(id)
    }

    pub async fn find_active_paired_device(
        &self,
        fingerprint: &str,
    ) -> Result<Option<PairedDeviceRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, fingerprint, device_label, scope_json, paired_at, last_seen_at,
                    status, principal_id, cert_serial
             FROM paired_devices
             WHERE fingerprint = ?1 AND status = 'active'",
                params!(fingerprint),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(paired_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn list_paired_devices(&self) -> Result<Vec<PairedDeviceRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, fingerprint, device_label, scope_json, paired_at, last_seen_at,
                    status, principal_id, cert_serial
             FROM paired_devices ORDER BY paired_at DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(paired_row_from_row(&row)?);
        }
        Ok(out)
    }

    pub async fn touch_paired_device(&self, fingerprint: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE paired_devices SET last_seen_at = ?1 WHERE fingerprint = ?2",
                params!(now, fingerprint),
            )
            .await?;
        Ok(())
    }

    pub async fn revoke_paired_device(
        &self,
        fingerprint: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE paired_devices SET status = 'revoked' WHERE fingerprint = ?1",
                params!(fingerprint),
            )
            .await?;
        self.add_revoked_fingerprint(fingerprint, reason).await?;
        self.conn
            .execute(
                "UPDATE internal_ca_certs SET status = 'revoked', revoked_at = ?1
             WHERE fingerprint = ?2 AND status != 'revoked'",
                params!(now, fingerprint),
            )
            .await?;
        Ok(())
    }

    pub async fn add_revoked_fingerprint(
        &self,
        fingerprint: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT OR IGNORE INTO revoked_fingerprints (fingerprint, revoked_at, reason)
             VALUES (?1, ?2, ?3)",
                params!(
                    fingerprint,
                    now,
                    reason.map(std::string::ToString::to_string)
                ),
            )
            .await?;
        Ok(())
    }

    /// `not_after` (UNIX seconds) for the active cert of the active paired
    /// device with `fingerprint`. Returns `None` when the fingerprint is
    /// unknown, revoked, or has no active cert in `internal_ca_certs`. Used
    /// by the auth interceptor to detect certs nearing expiry so it can
    /// stamp a renewal-due hint on the response.
    pub async fn paired_device_cert_expiry(&self, fingerprint: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT c.not_after
                 FROM paired_devices p
                 JOIN internal_ca_certs c ON c.serial = p.cert_serial
                 WHERE p.fingerprint = ?1
                   AND p.status = 'active'
                   AND c.status = 'active'",
                params!(fingerprint),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<i64>(0)?)),
            None => Ok(None),
        }
    }

    pub async fn is_fingerprint_revoked(&self, fingerprint: &str) -> Result<bool> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(1) FROM revoked_fingerprints WHERE fingerprint = ?1",
                params!(fingerprint),
            )
            .await?;
        let n: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(n > 0)
    }
}

fn paired_row_from_row(r: &libsql::Row) -> Result<PairedDeviceRow> {
    Ok(PairedDeviceRow {
        id: r.get::<i64>(0)?,
        fingerprint: r.get::<String>(1)?,
        device_label: r.get::<String>(2)?,
        scope_json: r.get::<String>(3)?,
        paired_at: r.get::<i64>(4)?,
        last_seen_at: r.get::<Option<i64>>(5)?,
        status: r.get::<String>(6)?,
        principal_id: r.get::<String>(7)?,
        cert_serial: r.get::<i64>(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn revoked_fingerprint_round_trip() {
        let db = test_db().await;
        assert!(!db.is_fingerprint_revoked("fp-A").await.expect("query"));
        db.add_revoked_fingerprint("fp-A", Some("manual"))
            .await
            .expect("insert");
        assert!(db.is_fingerprint_revoked("fp-A").await.expect("query"));
        db.add_revoked_fingerprint("fp-A", Some("again"))
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn paired_device_upsert_preserves_id() {
        let db = test_db().await;
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-1", "alice", 0, 100)
            .await
            .expect("cert");
        let id1 = db
            .insert_paired_device("fp-1", "alice", "{}", "alice", serial)
            .await
            .expect("first");
        let id2 = db
            .insert_paired_device("fp-1", "alice-renamed", "{}", "alice", serial)
            .await
            .expect("second upsert");
        assert_eq!(id1, id2, "upsert should reuse id");
        let row = db
            .find_active_paired_device("fp-1")
            .await
            .expect("q")
            .expect("present");
        assert_eq!(row.device_label, "alice-renamed");
    }

    #[tokio::test]
    async fn revoke_paired_device_blocks_lookup_and_marks_revocation_list() {
        let db = test_db().await;
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-2", "bob", 0, 100)
            .await
            .expect("cert");
        db.insert_paired_device("fp-2", "bob", "{}", "bob", serial)
            .await
            .expect("pair");
        assert!(db
            .find_active_paired_device("fp-2")
            .await
            .expect("q")
            .is_some());
        db.revoke_paired_device("fp-2", Some("lost laptop"))
            .await
            .expect("revoke");
        assert!(db
            .find_active_paired_device("fp-2")
            .await
            .expect("q")
            .is_none());
        assert!(db.is_fingerprint_revoked("fp-2").await.expect("q"));
    }

    #[tokio::test]
    async fn latest_active_cert_returns_newest_active_only() {
        let db = test_db().await;
        let s1 = db
            .insert_internal_cert("server", "OLD", "fp-old", "borgd", 0, 100)
            .await
            .expect("cert1");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let _s2 = db
            .insert_internal_cert("server", "NEW", "fp-new", "borgd", 0, 200)
            .await
            .expect("cert2");
        db.mark_cert_renewed(s1).await.expect("renew old");
        let latest = db
            .latest_active_cert("server")
            .await
            .expect("q")
            .expect("some");
        assert_eq!(latest.fingerprint, "fp-new");
    }
}
