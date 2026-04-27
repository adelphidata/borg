//! PAKE pairing-code lifecycle for gRPC client devices.
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use super::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeOutcome {
    Accepted,
    AlreadyConsumed,
    Expired,
    Unknown,
    RateLimited,
}

const MAX_ATTEMPTS_PER_CODE: i64 = 5;

pub fn hash_pairing_code(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    let digest = hasher.finalize();
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(&mut acc, "{:02x}", b);
        acc
    })
}

impl Database {
    pub fn create_pairing_code(&self, code: &str, intent: &str, ttl_secs: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let expires = now.saturating_add(ttl_secs);
        self.conn.execute(
            "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![hash_pairing_code(code), expires, intent, now],
        )?;
        Ok(())
    }

    pub fn consume_pairing_code(&self, code: &str) -> Result<ConsumeOutcome> {
        let hash = hash_pairing_code(code);
        let now = chrono::Utc::now().timestamp();

        let tx = self.conn.unchecked_transaction()?;

        let row: Option<(i64, Option<i64>, i64)> = tx
            .query_row(
                "SELECT expires_at, consumed_at, attempts
                 FROM pairing_codes WHERE code_hash = ?1",
                params![hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let outcome = match row {
            None => ConsumeOutcome::Unknown,
            Some((_, Some(_), _)) => ConsumeOutcome::AlreadyConsumed,
            Some((expires_at, None, _)) if expires_at <= now => ConsumeOutcome::Expired,
            Some((_, None, attempts)) if attempts >= MAX_ATTEMPTS_PER_CODE => {
                ConsumeOutcome::RateLimited
            }
            Some((_, None, _)) => {
                let n = tx.execute(
                    "UPDATE pairing_codes
                     SET consumed_at = ?1
                     WHERE code_hash = ?2 AND consumed_at IS NULL",
                    params![now, hash],
                )?;
                if n == 1 {
                    ConsumeOutcome::Accepted
                } else {
                    ConsumeOutcome::AlreadyConsumed
                }
            }
        };

        tx.commit()?;
        Ok(outcome)
    }

    pub fn bump_pairing_attempts(&self, code: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pairing_codes SET attempts = attempts + 1 WHERE code_hash = ?1",
            params![hash_pairing_code(code)],
        )?;
        Ok(())
    }

    pub fn count_active_pairing_codes(&self) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM pairing_codes
             WHERE consumed_at IS NULL AND expires_at > ?1",
            params![now],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn purge_stale_pairing_codes(&self) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - 86_400;
        let n = self.conn.execute(
            "DELETE FROM pairing_codes
             WHERE (consumed_at IS NOT NULL AND consumed_at < ?1)
                OR (consumed_at IS NULL AND expires_at < ?1)",
            params![cutoff],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::Arc;

    fn test_db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("in-mem db"))
            .expect("init db")
    }

    #[test]
    fn unknown_code_returns_unknown() {
        let db = test_db();
        assert_eq!(
            db.consume_pairing_code("never-minted").expect("q"),
            ConsumeOutcome::Unknown
        );
    }

    #[test]
    fn expired_code_rejected() {
        let db = test_db();
        let hash = hash_pairing_code("expired-code");
        db.conn()
            .execute(
                "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, 'pair', ?3)",
                params![hash, 1_000_i64, 999_i64],
            )
            .expect("insert");
        assert_eq!(
            db.consume_pairing_code("expired-code").expect("q"),
            ConsumeOutcome::Expired
        );
    }

    #[test]
    fn first_consume_wins_replay_rejected() {
        let db = test_db();
        db.create_pairing_code("ABCD-1234", "pair", 300).expect("mint");
        assert_eq!(
            db.consume_pairing_code("ABCD-1234").expect("first"),
            ConsumeOutcome::Accepted
        );
        assert_eq!(
            db.consume_pairing_code("ABCD-1234").expect("replay"),
            ConsumeOutcome::AlreadyConsumed
        );
    }

    #[test]
    fn rate_limit_kicks_in_after_max_attempts() {
        let db = test_db();
        db.create_pairing_code("ZZZZ-9999", "pair", 300).expect("mint");
        for _ in 0..MAX_ATTEMPTS_PER_CODE {
            db.bump_pairing_attempts("ZZZZ-9999").expect("bump");
        }
        assert_eq!(
            db.consume_pairing_code("ZZZZ-9999").expect("q"),
            ConsumeOutcome::RateLimited,
        );
    }

    #[test]
    fn concurrent_consume_only_one_winner() {
        use std::sync::Mutex as StdMutex;
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("race.db");
        let db = Database::from_connection(Connection::open(&path).expect("open primary"))
            .expect("init");
        db.create_pairing_code("RACE-0001", "pair", 300).expect("mint");
        let shared = Arc::new(StdMutex::new(db));

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = shared.clone();
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                b.wait();
                let g = s.lock().expect("lock");
                g.consume_pairing_code("RACE-0001").expect("consume")
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().expect("join")).collect();
        let accepted = outcomes
            .iter()
            .filter(|o| matches!(o, ConsumeOutcome::Accepted))
            .count();
        let already = outcomes
            .iter()
            .filter(|o| matches!(o, ConsumeOutcome::AlreadyConsumed))
            .count();
        assert_eq!(accepted, 1);
        assert_eq!(already + accepted, outcomes.len());
    }

    #[test]
    fn count_active_excludes_expired_and_consumed() {
        let db = test_db();
        db.create_pairing_code("LIVE-1", "pair", 300).expect("live");
        db.create_pairing_code("USED-1", "pair", 300).expect("used");
        db.consume_pairing_code("USED-1").expect("consume");
        let hash = hash_pairing_code("EXPIRED-1");
        db.conn()
            .execute(
                "INSERT INTO pairing_codes (code_hash, expires_at, intent, created_at)
             VALUES (?1, ?2, 'pair', ?3)",
                params![hash, 1_i64, 0_i64],
            )
            .expect("expired");
        assert_eq!(db.count_active_pairing_codes().expect("count"), 1);
    }
}
