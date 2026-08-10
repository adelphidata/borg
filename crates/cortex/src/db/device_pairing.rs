//! PAKE pairing-code lifecycle for gRPC client devices.
use anyhow::Result;
use libsql::params;
use sha2::{Digest, Sha256};

use super::Database;
use crate::util::hex::hex_encode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeOutcome {
    Accepted,
    AlreadyConsumed,
    Expired,
    Unknown,
    RateLimited,
}

/// Brute-force ceiling per pairing code: codes are short and human-typed, so
/// the guess budget must stay far below the code's entropy.
const MAX_ATTEMPTS_PER_CODE: i64 = 5;

pub fn hash_pairing_code(code: &str) -> String {
    hex_encode(&Sha256::digest(code.as_bytes()))
}

impl Database {
    pub async fn create_pairing_code(&self, code: &str, intent: &str, ttl_secs: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let expires = now.saturating_add(ttl_secs);
        self.conn
            .execute(
                "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params!(hash_pairing_code(code), expires, intent, now),
            )
            .await?;
        Ok(())
    }

    pub async fn consume_pairing_code(&self, code: &str) -> Result<ConsumeOutcome> {
        let hash = hash_pairing_code(code);
        let now = chrono::Utc::now().timestamp();

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<ConsumeOutcome> = async {
            let row: Option<(i64, Option<i64>, i64)> = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT expires_at, consumed_at, attempts
                 FROM pairing_codes WHERE code_hash = ?1",
                        params!(hash.clone()),
                    )
                    .await?;
                match rows.next().await? {
                    Some(r) => Some((r.get::<i64>(0)?, r.get::<Option<i64>>(1)?, r.get::<i64>(2)?)),
                    None => None,
                }
            };

            let outcome = match row {
                None => ConsumeOutcome::Unknown,
                Some((_, Some(_), _)) => ConsumeOutcome::AlreadyConsumed,
                Some((expires_at, None, _)) if expires_at <= now => ConsumeOutcome::Expired,
                Some((_, None, attempts)) if attempts >= MAX_ATTEMPTS_PER_CODE => {
                    ConsumeOutcome::RateLimited
                }
                Some((_, None, _)) => {
                    let n = self
                        .conn
                        .execute(
                            "UPDATE pairing_codes
                     SET consumed_at = ?1
                     WHERE code_hash = ?2 AND consumed_at IS NULL",
                            params!(now, hash),
                        )
                        .await?;
                    if n == 1 {
                        ConsumeOutcome::Accepted
                    } else {
                        ConsumeOutcome::AlreadyConsumed
                    }
                }
            };

            Ok(outcome)
        }
        .await;
        self.finish_immediate_tx(result, "device_pairing consume")
            .await
    }

    pub async fn bump_pairing_attempts(&self, code: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE pairing_codes SET attempts = attempts + 1 WHERE code_hash = ?1",
                params!(hash_pairing_code(code)),
            )
            .await?;
        Ok(())
    }

    pub async fn count_active_pairing_codes(&self) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(1) FROM pairing_codes
             WHERE consumed_at IS NULL AND expires_at > ?1",
                params!(now),
            )
            .await?;
        let n: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(n)
    }

    pub async fn purge_stale_pairing_codes(&self) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - 86_400;
        let n = self
            .conn
            .execute(
                "DELETE FROM pairing_codes
             WHERE (consumed_at IS NOT NULL AND consumed_at < ?1)
                OR (consumed_at IS NULL AND expires_at < ?1)",
                params!(cutoff),
            )
            .await?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn unknown_code_returns_unknown() {
        let db = test_db().await;
        assert_eq!(
            db.consume_pairing_code("never-minted").await.expect("q"),
            ConsumeOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn expired_code_rejected() {
        let db = test_db().await;
        let hash = hash_pairing_code("expired-code");
        db.conn()
            .execute(
                "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, 'pair', ?3)",
                params!(hash, 1_000_i64, 999_i64),
            )
            .await
            .expect("insert");
        assert_eq!(
            db.consume_pairing_code("expired-code").await.expect("q"),
            ConsumeOutcome::Expired
        );
    }

    #[tokio::test]
    async fn first_consume_wins_replay_rejected() {
        let db = test_db().await;
        db.create_pairing_code("ABCD-1234", "pair", 300)
            .await
            .expect("mint");
        assert_eq!(
            db.consume_pairing_code("ABCD-1234").await.expect("first"),
            ConsumeOutcome::Accepted
        );
        assert_eq!(
            db.consume_pairing_code("ABCD-1234").await.expect("replay"),
            ConsumeOutcome::AlreadyConsumed
        );
    }

    #[tokio::test]
    async fn rate_limit_kicks_in_after_max_attempts() {
        let db = test_db().await;
        db.create_pairing_code("ZZZZ-9999", "pair", 300)
            .await
            .expect("mint");
        for _ in 0..MAX_ATTEMPTS_PER_CODE {
            db.bump_pairing_attempts("ZZZZ-9999").await.expect("bump");
        }
        assert_eq!(
            db.consume_pairing_code("ZZZZ-9999").await.expect("q"),
            ConsumeOutcome::RateLimited,
        );
    }

    #[tokio::test]
    async fn count_active_excludes_expired_and_consumed() {
        let db = test_db().await;
        db.create_pairing_code("LIVE-1", "pair", 300)
            .await
            .expect("live");
        db.create_pairing_code("USED-1", "pair", 300)
            .await
            .expect("used");
        db.consume_pairing_code("USED-1").await.expect("consume");
        let hash = hash_pairing_code("EXPIRED-1");
        db.conn()
            .execute(
                "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, 'pair', ?3)",
                params!(hash, 1_i64, 0_i64),
            )
            .await
            .expect("expired");
        assert_eq!(db.count_active_pairing_codes().await.expect("count"), 1);
    }
}
