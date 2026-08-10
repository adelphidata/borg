use anyhow::Result;
use libsql::params;

use super::device_pairing::hash_pairing_code;
use super::models::{ApprovedSenderRow, PairingRequestRow};
use super::Database;

impl Database {
    pub async fn create_pairing_request(
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
        // Hash before insert — DB must never contain the plaintext code.
        // Lowercase first so lookups normalize identically.
        let code_hash = hash_pairing_code(&code.to_lowercase());
        self.conn.execute(
            "INSERT INTO pairing_requests (id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7)",
            params!(id.clone(), channel_name, sender_id, code_hash, display_name.map(std::string::ToString::to_string), now, expires_at),
        ).await?;
        Ok(id)
    }

    fn pairing_row_from_row(row: &libsql::Row) -> Result<PairingRequestRow> {
        Ok(PairingRequestRow {
            id: row.get::<String>(0)?,
            channel_name: row.get::<String>(1)?,
            sender_id: row.get::<String>(2)?,
            code: row.get::<String>(3)?,
            status: row.get::<String>(4)?,
            display_name: row.get::<Option<String>>(5)?,
            created_at: row.get::<i64>(6)?,
            expires_at: row.get::<i64>(7)?,
            approved_at: row.get::<Option<i64>>(8)?,
        })
    }

    fn approved_sender_row_from_row(row: &libsql::Row) -> Result<ApprovedSenderRow> {
        Ok(ApprovedSenderRow {
            id: row.get::<i64>(0)?,
            channel_name: row.get::<String>(1)?,
            sender_id: row.get::<String>(2)?,
            display_name: row.get::<Option<String>>(3)?,
            approved_at: row.get::<i64>(4)?,
        })
    }

    pub async fn find_pending_pairing(
        &self,
        channel_name: &str,
        code: &str,
    ) -> Result<Option<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        let code_hash = hash_pairing_code(&code.to_lowercase());
        let mut rows = self.conn.query(
            "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND code_hash = ?2 AND status = 'pending' AND expires_at > ?3",
            params!(channel_name, code_hash, now),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::pairing_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn find_pending_for_sender(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<Option<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.conn.query(
            "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND sender_id = ?2 AND status = 'pending' AND expires_at > ?3
             ORDER BY created_at DESC LIMIT 1",
            params!(channel_name, sender_id, now),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::pairing_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn approve_pairing(
        &self,
        channel_name: &str,
        code: &str,
    ) -> Result<PairingRequestRow> {
        // Pairing codes are now lowercase BIP-39 phrases (e.g.
        // `tg/voyage-kitten-orbit-marble-cinder-pluto`); normalize to
        // lowercase before hashing so lookups match whatever the user typed.
        let code = code.to_lowercase();
        let code_hash = hash_pairing_code(&code);
        let now = chrono::Utc::now().timestamp();

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<PairingRequestRow> = async {
            let request = {
                let mut rows = self.conn.query(
                    "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
                 FROM pairing_requests
                 WHERE channel_name = ?1 AND code_hash = ?2 AND status = 'pending' AND expires_at > ?3",
                    params!(channel_name, code_hash.clone(), now),
                ).await?;
                match rows.next().await? {
                    Some(row) => Self::pairing_row_from_row(&row)?,
                    None => {
                        anyhow::bail!(
                            "No pending pairing request found for channel '{channel_name}' with the supplied code"
                        );
                    }
                }
            };

            self.conn.execute(
                "UPDATE pairing_requests SET status = 'approved', approved_at = ?1 WHERE id = ?2",
                params!(now, request.id.clone()),
            )
            .await?;

            self.conn.execute(
                "INSERT INTO approved_senders (channel_name, sender_id, display_name, approved_at, pairing_request_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(channel_name, sender_id) DO UPDATE SET
                approved_at = ?4, pairing_request_id = ?5",
                params!(
                    request.channel_name.clone(),
                    request.sender_id.clone(),
                    request.display_name.clone(),
                    now,
                    request.id.clone(),
                ),
            ).await?;

            Ok(request)
        }.await;
        let request = self.finish_immediate_tx(result, "pairing approve").await?;
        Ok(PairingRequestRow {
            status: "approved".into(),
            approved_at: Some(now),
            ..request
        })
    }

    /// Mark every pending pairing request for `(channel, sender)` as
    /// `expired`. Used by the gateway when a sender re-asks for a code —
    /// the old hashed row can no longer be re-displayed, so it's invalidated
    /// before a fresh code is minted.
    pub async fn expire_pending_for_sender(
        &self,
        channel_name: &str,
        sender_id: &str,
    ) -> Result<usize> {
        let changes = self
            .conn
            .execute(
                "UPDATE pairing_requests
                    SET status = 'expired'
                    WHERE channel_name = ?1 AND sender_id = ?2 AND status = 'pending'",
                params!(channel_name, sender_id),
            )
            .await?;
        Ok(changes as usize)
    }

    /// Remove expired pending pairing requests.
    pub async fn cleanup_expired_pairings(&self) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let deleted = self
            .conn
            .execute(
                "DELETE FROM pairing_requests WHERE status = 'pending' AND expires_at <= ?1",
                params!(now),
            )
            .await?;
        Ok(deleted as usize)
    }

    pub async fn is_sender_approved(&self, channel_name: &str, sender_id: &str) -> Result<bool> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM approved_senders WHERE channel_name = ?1 AND sender_id = ?2",
                params!(channel_name, sender_id),
            )
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count > 0)
    }

    /// Count pairing requests created by a sender within the last `window_secs` seconds.
    pub async fn count_pairing_attempts(
        &self,
        channel_name: &str,
        sender_id: &str,
        window_secs: i64,
    ) -> Result<u32> {
        let cutoff = chrono::Utc::now().timestamp() - window_secs;
        let mut rows = self.conn.query(
            "SELECT COUNT(*) FROM pairing_requests WHERE channel_name = ?1 AND sender_id = ?2 AND created_at > ?3",
            params!(channel_name, sender_id, cutoff),
        ).await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub async fn revoke_sender(&self, channel_name: &str, sender_id: &str) -> Result<bool> {
        let changes = self
            .conn
            .execute(
                "DELETE FROM approved_senders WHERE channel_name = ?1 AND sender_id = ?2",
                params!(channel_name, sender_id),
            )
            .await?;
        Ok(changes > 0)
    }

    /// Find a pending pairing request by code alone (across all channels).
    pub async fn find_pending_by_code(&self, code: &str) -> Result<Option<PairingRequestRow>> {
        // Pairing codes are lowercase BIP-39 phrases; normalize and hash
        // before lookup so the DB never sees the plaintext code.
        let code_hash = hash_pairing_code(&code.to_lowercase());
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.conn.query(
            "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE code_hash = ?1 AND status = 'pending' AND expires_at > ?2
             ORDER BY created_at DESC LIMIT 1",
            params!(code_hash, now),
        ).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::pairing_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn list_pairings(
        &self,
        channel_name: Option<&str>,
    ) -> Result<Vec<PairingRequestRow>> {
        let now = chrono::Utc::now().timestamp();
        if let Some(ch) = channel_name {
            self.list_pairings_for_channel(ch, now).await
        } else {
            self.list_pairings_all(now).await
        }
    }

    async fn list_pairings_for_channel(
        &self,
        ch: &str,
        now: i64,
    ) -> Result<Vec<PairingRequestRow>> {
        let mut rows = self.conn.query(
            "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE channel_name = ?1 AND status = 'pending' AND expires_at > ?2
             ORDER BY created_at DESC",
            params!(ch, now),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::pairing_row_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_pairings_all(&self, now: i64) -> Result<Vec<PairingRequestRow>> {
        let mut rows = self.conn.query(
            "SELECT id, channel_name, sender_id, code_hash, status, display_name, created_at, expires_at, approved_at
             FROM pairing_requests
             WHERE status = 'pending' AND expires_at > ?1
             ORDER BY created_at DESC",
            params!(now),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::pairing_row_from_row(&row)?);
        }
        Ok(out)
    }

    pub async fn list_approved_senders(
        &self,
        channel_name: Option<&str>,
    ) -> Result<Vec<ApprovedSenderRow>> {
        if let Some(ch) = channel_name {
            self.list_approved_senders_for_channel(ch).await
        } else {
            self.list_approved_senders_all().await
        }
    }

    async fn list_approved_senders_for_channel(&self, ch: &str) -> Result<Vec<ApprovedSenderRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, channel_name, sender_id, display_name, approved_at
             FROM approved_senders WHERE channel_name = ?1 ORDER BY approved_at DESC",
                params!(ch),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::approved_sender_row_from_row(&row)?);
        }
        Ok(out)
    }

    async fn list_approved_senders_all(&self) -> Result<Vec<ApprovedSenderRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, channel_name, sender_id, display_name, approved_at
             FROM approved_senders ORDER BY approved_at DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::approved_sender_row_from_row(&row)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn test_create_and_find_pending_pairing() {
        let db = test_db().await;
        let id = db
            .create_pairing_request("telegram", "user1", "ABC123", Some("Alice"), 3600)
            .await
            .unwrap();
        assert!(!id.is_empty());

        let found = db.find_pending_pairing("telegram", "ABC123").await.unwrap();
        assert!(found.is_some());
        let req = found.unwrap();
        assert_eq!(req.channel_name, "telegram");
        assert_eq!(req.sender_id, "user1");
        // Codes are stored as SHA-256(lowercase plaintext) — never raw plaintext.
        assert_eq!(req.code, hash_pairing_code("abc123"));
        assert_ne!(req.code, "abc123");
        assert_eq!(req.status, "pending");
        assert_eq!(req.display_name.as_deref(), Some("Alice"));
    }

    /// Regression guard: the `pairing_requests.code_hash` column must never
    /// contain the plaintext pairing code. Pre-fix the column held the raw
    /// 6-word BIP-39 phrase, so a DB-read leak meant attacker-replayable codes.
    #[tokio::test]
    async fn pairing_requests_never_store_plaintext_code() {
        let db = test_db().await;
        let plaintext = "voyage-kitten-orbit-marble-cinder-pluto";
        db.create_pairing_request("telegram", "alice", plaintext, None, 3600)
            .await
            .unwrap();
        let mut rows = db
            .conn
            .query("SELECT code_hash FROM pairing_requests", ())
            .await
            .unwrap();
        let row = rows
            .next()
            .await
            .unwrap()
            .expect("row inserted by create_pairing_request");
        let stored: String = row.get(0).unwrap();
        assert_ne!(stored, plaintext, "DB stored plaintext pairing code");
        assert_eq!(stored, hash_pairing_code(plaintext));
        assert_eq!(stored.len(), 64, "expected hex-encoded SHA-256");
    }

    #[tokio::test]
    async fn test_find_pending_pairing_expired() {
        let db = test_db().await;
        // TTL of 0 means it expires immediately
        db.create_pairing_request("telegram", "user1", "EXPIRED", None, 0)
            .await
            .unwrap();

        let found = db
            .find_pending_pairing("telegram", "EXPIRED")
            .await
            .unwrap();
        assert!(found.is_none(), "expired pairing should not be found");
    }

    #[tokio::test]
    async fn test_find_pending_for_sender() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "CODE1", None, 3600)
            .await
            .unwrap();

        let found = db
            .find_pending_for_sender("telegram", "user1")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().code, hash_pairing_code("code1"));

        // Different sender returns None
        let other = db
            .find_pending_for_sender("telegram", "nobody")
            .await
            .unwrap();
        assert!(other.is_none());
    }

    #[tokio::test]
    async fn test_approve_pairing() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "CODE1", Some("Alice"), 3600)
            .await
            .unwrap();

        assert!(!db.is_sender_approved("telegram", "user1").await.unwrap());

        let approved = db.approve_pairing("telegram", "CODE1").await.unwrap();
        assert_eq!(approved.status, "approved");
        assert!(approved.approved_at.is_some());

        assert!(db.is_sender_approved("telegram", "user1").await.unwrap());

        // No longer findable as pending
        let pending = db.find_pending_pairing("telegram", "CODE1").await.unwrap();
        assert!(pending.is_none());
    }

    #[tokio::test]
    async fn test_approve_pairing_not_found() {
        let db = test_db().await;
        let result = db.approve_pairing("telegram", "NONEXISTENT").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_is_sender_approved_false_initially() {
        let db = test_db().await;
        assert!(!db.is_sender_approved("telegram", "nobody").await.unwrap());
    }

    #[tokio::test]
    async fn test_revoke_sender() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "CODE1", None, 3600)
            .await
            .unwrap();
        db.approve_pairing("telegram", "CODE1").await.unwrap();
        assert!(db.is_sender_approved("telegram", "user1").await.unwrap());

        let revoked = db.revoke_sender("telegram", "user1").await.unwrap();
        assert!(revoked);
        assert!(!db.is_sender_approved("telegram", "user1").await.unwrap());

        // Second revoke returns false
        let revoked_again = db.revoke_sender("telegram", "user1").await.unwrap();
        assert!(!revoked_again);
    }

    #[tokio::test]
    async fn test_cleanup_expired_pairings() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "VALID", None, 3600)
            .await
            .unwrap();
        db.create_pairing_request("telegram", "user2", "EXPIRED", None, 0)
            .await
            .unwrap();

        let cleaned = db.cleanup_expired_pairings().await.unwrap();
        assert_eq!(cleaned, 1);

        // Valid one should still be findable
        assert!(db
            .find_pending_pairing("telegram", "VALID")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn test_count_pairing_attempts() {
        let db = test_db().await;
        for i in 0..3 {
            db.create_pairing_request("telegram", "user1", &format!("C{i}"), None, 3600)
                .await
                .unwrap();
        }

        let count = db
            .count_pairing_attempts("telegram", "user1", 3600)
            .await
            .unwrap();
        assert_eq!(count, 3);

        // Different sender should be 0
        let other = db
            .count_pairing_attempts("telegram", "nobody", 3600)
            .await
            .unwrap();
        assert_eq!(other, 0);
    }

    #[tokio::test]
    async fn test_list_pairings_and_approved_senders() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "T1", Some("Alice"), 3600)
            .await
            .unwrap();
        db.create_pairing_request("slack", "user2", "S1", Some("Bob"), 3600)
            .await
            .unwrap();

        // Approve the telegram one
        db.approve_pairing("telegram", "T1").await.unwrap();

        // Pending pairings: only slack one remains
        let pending = db.list_pairings(None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].channel_name, "slack");

        // Approved senders for telegram
        let approved = db.list_approved_senders(Some("telegram")).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].sender_id, "user1");

        // Approved senders for slack: none
        let slack_approved = db.list_approved_senders(Some("slack")).await.unwrap();
        assert!(slack_approved.is_empty());
    }

    #[tokio::test]
    async fn test_find_pending_by_code() {
        let db = test_db().await;
        db.create_pairing_request("telegram", "user1", "MYCODE", None, 3600)
            .await
            .unwrap();

        // Should find by code alone (case-insensitive via uppercasing)
        let found = db.find_pending_by_code("mycode").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().channel_name, "telegram");
    }
}
