//! Persistence for the daemon's internal CA, paired devices, and revocation list.
use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};

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
    pub fn insert_internal_cert(
        &self,
        kind: &str,
        pem: &str,
        fingerprint: &str,
        subject: &str,
        not_before: i64,
        not_after: i64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO internal_ca_certs
             (kind, pem, fingerprint, subject, not_before, not_after, status, issued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![kind, pem, fingerprint, subject, not_before, not_after, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn latest_active_cert(&self, kind: &str) -> Result<Option<InternalCertRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT serial, kind, pem, fingerprint, subject, not_before, not_after,
                    status, issued_at, revoked_at
             FROM internal_ca_certs
             WHERE kind = ?1 AND status = 'active'
             ORDER BY issued_at DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![kind], |r| {
                Ok(InternalCertRow {
                    serial: r.get(0)?,
                    kind: r.get(1)?,
                    pem: r.get(2)?,
                    fingerprint: r.get(3)?,
                    subject: r.get(4)?,
                    not_before: r.get(5)?,
                    not_after: r.get(6)?,
                    status: r.get(7)?,
                    issued_at: r.get(8)?,
                    revoked_at: r.get(9)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn mark_cert_renewed(&self, serial: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE internal_ca_certs SET status = 'renewed' WHERE serial = ?1",
            params![serial],
        )?;
        if n == 0 {
            return Err(anyhow!("no internal cert with serial {}", serial));
        }
        Ok(())
    }

    pub fn mark_cert_revoked(&self, serial: i64, reason: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE internal_ca_certs
             SET status = 'revoked', revoked_at = ?1
             WHERE serial = ?2 AND status != 'revoked'",
            params![now, serial],
        )?;
        if let Some(cert) = self
            .conn
            .query_row(
                "SELECT fingerprint FROM internal_ca_certs WHERE serial = ?1",
                params![serial],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            self.add_revoked_fingerprint(&cert, reason)?;
        }
        Ok(())
    }

    pub fn insert_paired_device(
        &self,
        fingerprint: &str,
        device_label: &str,
        scope_json: &str,
        principal_id: &str,
        cert_serial: i64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO paired_devices
             (fingerprint, device_label, scope_json, paired_at, status, principal_id, cert_serial)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6)
             ON CONFLICT(fingerprint) DO UPDATE SET
                device_label = excluded.device_label,
                scope_json   = excluded.scope_json,
                principal_id = excluded.principal_id,
                cert_serial  = excluded.cert_serial,
                status       = 'active'",
            params![
                fingerprint,
                device_label,
                scope_json,
                now,
                principal_id,
                cert_serial
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM paired_devices WHERE fingerprint = ?1",
            params![fingerprint],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn find_active_paired_device(&self, fingerprint: &str) -> Result<Option<PairedDeviceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, fingerprint, device_label, scope_json, paired_at, last_seen_at,
                    status, principal_id, cert_serial
             FROM paired_devices
             WHERE fingerprint = ?1 AND status = 'active'",
        )?;
        let row = stmt
            .query_row(params![fingerprint], map_paired_row)
            .optional()?;
        Ok(row)
    }

    pub fn list_paired_devices(&self) -> Result<Vec<PairedDeviceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, fingerprint, device_label, scope_json, paired_at, last_seen_at,
                    status, principal_id, cert_serial
             FROM paired_devices ORDER BY paired_at DESC",
        )?;
        let rows = stmt.query_map([], map_paired_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn touch_paired_device(&self, fingerprint: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE paired_devices SET last_seen_at = ?1 WHERE fingerprint = ?2",
            params![now, fingerprint],
        )?;
        Ok(())
    }

    pub fn revoke_paired_device(&self, fingerprint: &str, reason: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE paired_devices SET status = 'revoked' WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        self.add_revoked_fingerprint(fingerprint, reason)?;
        self.conn.execute(
            "UPDATE internal_ca_certs SET status = 'revoked', revoked_at = ?1
             WHERE fingerprint = ?2 AND status != 'revoked'",
            params![now, fingerprint],
        )?;
        Ok(())
    }

    pub fn add_revoked_fingerprint(&self, fingerprint: &str, reason: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR IGNORE INTO revoked_fingerprints (fingerprint, revoked_at, reason)
             VALUES (?1, ?2, ?3)",
            params![fingerprint, now, reason],
        )?;
        Ok(())
    }

    /// `not_after` (UNIX seconds) for the active cert of the active paired
    /// device with `fingerprint`. Returns `None` when the fingerprint is
    /// unknown, revoked, or has no active cert in `internal_ca_certs`. Used
    /// by the auth interceptor to detect certs nearing expiry so it can
    /// stamp a renewal-due hint on the response.
    pub fn paired_device_cert_expiry(&self, fingerprint: &str) -> Result<Option<i64>> {
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT c.not_after
                 FROM paired_devices p
                 JOIN internal_ca_certs c ON c.serial = p.cert_serial
                 WHERE p.fingerprint = ?1
                   AND p.status = 'active'
                   AND c.status = 'active'",
                params![fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row)
    }

    pub fn is_fingerprint_revoked(&self, fingerprint: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM revoked_fingerprints WHERE fingerprint = ?1",
            params![fingerprint],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

fn map_paired_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PairedDeviceRow> {
    Ok(PairedDeviceRow {
        id: r.get(0)?,
        fingerprint: r.get(1)?,
        device_label: r.get(2)?,
        scope_json: r.get(3)?,
        paired_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        status: r.get(6)?,
        principal_id: r.get(7)?,
        cert_serial: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("in-mem db"))
            .expect("init db")
    }

    #[test]
    fn revoked_fingerprint_round_trip() {
        let db = test_db();
        assert!(!db.is_fingerprint_revoked("fp-A").expect("query"));
        db.add_revoked_fingerprint("fp-A", Some("manual"))
            .expect("insert");
        assert!(db.is_fingerprint_revoked("fp-A").expect("query"));
        db.add_revoked_fingerprint("fp-A", Some("again"))
            .expect("idempotent");
    }

    #[test]
    fn paired_device_upsert_preserves_id() {
        let db = test_db();
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-1", "alice", 0, 100)
            .expect("cert");
        let id1 = db
            .insert_paired_device("fp-1", "alice", "{}", "alice", serial)
            .expect("first");
        let id2 = db
            .insert_paired_device("fp-1", "alice-renamed", "{}", "alice", serial)
            .expect("second upsert");
        assert_eq!(id1, id2, "upsert should reuse id");
        let row = db
            .find_active_paired_device("fp-1")
            .expect("q")
            .expect("present");
        assert_eq!(row.device_label, "alice-renamed");
    }

    #[test]
    fn revoke_paired_device_blocks_lookup_and_marks_revocation_list() {
        let db = test_db();
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-2", "bob", 0, 100)
            .expect("cert");
        db.insert_paired_device("fp-2", "bob", "{}", "bob", serial)
            .expect("pair");
        assert!(db.find_active_paired_device("fp-2").expect("q").is_some());
        db.revoke_paired_device("fp-2", Some("lost laptop"))
            .expect("revoke");
        assert!(db.find_active_paired_device("fp-2").expect("q").is_none());
        assert!(db.is_fingerprint_revoked("fp-2").expect("q"));
    }

    #[test]
    fn latest_active_cert_returns_newest_active_only() {
        let db = test_db();
        let s1 = db
            .insert_internal_cert("server", "OLD", "fp-old", "borgd", 0, 100)
            .expect("cert1");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _s2 = db
            .insert_internal_cert("server", "NEW", "fp-new", "borgd", 0, 200)
            .expect("cert2");
        db.mark_cert_renewed(s1).expect("renew old");
        let latest = db.latest_active_cert("server").expect("q").expect("some");
        assert_eq!(latest.fingerprint, "fp-new");
    }
}
