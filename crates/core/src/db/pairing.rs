use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::models::{ApprovedSenderRow, PairingRequestRow};
use super::Database;

impl Database {
    // ── Pairing CRUD ──

    pub fn create_pairing_request(
        &self,
        channel_name: &str,
        sender_id: &str,
        code: &str,
        display_name: Option<&str>,
        ttl_secs: i64,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let expires_at = now + ttl_secs;
        self.conn.execute(
            "INSERT INTO pairing_requests (id, channel_name, sender_id, code, status, display_name, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7)",
            params![id, channel_name, sender_id, code, display_name, now, expires_at],
        )?;
        Ok(id)
    }

    fn map_pairing_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PairingRequestRow> {
        Ok(PairingRequestRow {
            id: row.get(0)?,
            channel_name: row.get(1)?,
            sender_id: row.get(2)?,
            code: row.get(3)?,
            status: row.get(4)?,
            display_name: row.get(5)?,
            created_at: row.get(6)?,
            expires_at: row.get(7)?,
            approved_at: row.get(8)?,
        })
    }

    fn map_approved_sender_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovedSenderRow> {
        Ok(ApprovedSenderRow {
            id: row.get(0)?,
            channel_name: row.get(1)?,
            sender_id: row.get(2)?,
            display_name: row.get(3)?,
            approved_at: row.get(4)?,
        })
    }

    pub fn find_pending_pairing(
        &self,
        channel_name: &str,
        code: &str,
    ) -> Result<Option<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND code = ?2 AND status = 'pending' AND expires_at > ?3",
        )?;
        let row = stmt
            .query_row(params![channel_name, code, now], Self::map_pairing_row)
            .optional()?;
        Ok(row)
    }

    pub fn find_pending_for_sender(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<Option<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND sender_id = ?2 AND status = 'pending' AND expires_at > ?3
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![channel_name, sender_id, now], Self::map_pairing_row)
            .optional()?;
        Ok(row)
    }

    pub fn approve_pairing(&self, channel_name: &str, code: &str) -> Result<PairingRequestRow> {
        let code = code.to_uppercase();
        let now = chrono::Utc::now().timestamp();

        let tx = self.conn.unchecked_transaction()?;

        // Find the pending request within the transaction
        let request = {
            let mut stmt = tx.prepare(
                "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
                 FROM pairing_requests
                 WHERE channel_name = ?1 AND code = ?2 AND status = 'pending' AND expires_at > ?3",
            )?;
            stmt.query_row(params![channel_name, code, now], Self::map_pairing_row)
                .optional()?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No pending pairing request found for channel '{channel_name}' with code '{code}'"
                    )
                })?
        };

        tx.execute(
            "UPDATE pairing_requests SET status = 'approved', approved_at = ?1 WHERE id = ?2",
            params![now, request.id],
        )?;

        tx.execute(
            "INSERT INTO approved_senders (channel_name, sender_id, display_name, approved_at, pairing_request_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(channel_name, sender_id) DO UPDATE SET
                approved_at = ?4, pairing_request_id = ?5",
            params![
                request.channel_name,
                request.sender_id,
                request.display_name,
                now,
                request.id,
            ],
        )?;

        tx.commit()?;

        Ok(PairingRequestRow {
            status: "approved".into(),
            approved_at: Some(now),
            ..request
        })
    }

    /// Remove expired pending pairing requests.
    pub fn cleanup_expired_pairings(&self) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let deleted = self.conn.execute(
            "DELETE FROM pairing_requests WHERE status = 'pending' AND expires_at <= ?1",
            params![now],
        )?;
        Ok(deleted)
    }

    pub fn is_sender_approved(&self, channel_name: &str, sender_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM approved_senders WHERE channel_name = ?1 AND sender_id = ?2",
            params![channel_name, sender_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Count pairing requests created by a sender within the last `window_secs` seconds.
    pub fn count_pairing_attempts(
        &self,
        channel_name: &str,
        sender_id: &str,
        window_secs: i64,
    ) -> Result<u32> {
        let cutoff = chrono::Utc::now().timestamp() - window_secs;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pairing_requests WHERE channel_name = ?1 AND sender_id = ?2 AND created_at > ?3",
            params![channel_name, sender_id, cutoff],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub fn revoke_sender(&self, channel_name: &str, sender_id: &str) -> Result<bool> {
        let changes = self.conn.execute(
            "DELETE FROM approved_senders WHERE channel_name = ?1 AND sender_id = ?2",
            params![channel_name, sender_id],
        )?;
        Ok(changes > 0)
    }

    /// Find a pending pairing request by code alone (across all channels).
    pub fn find_pending_by_code(&self, code: &str) -> Result<Option<PairingRequestRow>> {
        let code = code.to_uppercase();
        let now = chrono::Utc::now().timestamp();
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE code = ?1 AND status = 'pending' AND expires_at > ?2
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![code, now], Self::map_pairing_row)
            .optional()?;
        Ok(row)
    }

    pub fn list_pairings(&self, channel_name: Option<&str>) -> Result<Vec<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        if let Some(ch) = channel_name {
            self.list_pairings_for_channel(ch, now)
        } else {
            self.list_pairings_all(now)
        }
    }

    fn list_pairings_for_channel(&self, ch: &str, now: i64) -> Result<Vec<PairingRequestRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND status = 'pending' AND expires_at > ?2
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![ch, now], Self::map_pairing_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn list_pairings_all(&self, now: i64) -> Result<Vec<PairingRequestRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, code, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE status = 'pending' AND expires_at > ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![now], Self::map_pairing_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_approved_senders(
        &self,
        channel_name: Option<&str>,
    ) -> Result<Vec<ApprovedSenderRow>> {
        if let Some(ch) = channel_name {
            self.list_approved_senders_for_channel(ch)
        } else {
            self.list_approved_senders_all()
        }
    }

    fn list_approved_senders_for_channel(&self, ch: &str) -> Result<Vec<ApprovedSenderRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, display_name, approved_at
             FROM approved_senders WHERE channel_name = ?1 ORDER BY approved_at DESC",
        )?;
        let rows = stmt
            .query_map(params![ch], Self::map_approved_sender_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn list_approved_senders_all(&self) -> Result<Vec<ApprovedSenderRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_name, sender_id, display_name, approved_at
             FROM approved_senders ORDER BY approved_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_approved_sender_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
