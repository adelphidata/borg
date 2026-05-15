//! Persistent cache for `web_search` results, keyed by `(provider, query_hash)`.
//!
//! Lookups are cheap (indexed primary key); freshness is decided by the caller
//! via `fetched_at` so the same row can serve hits in different "live vs cached"
//! modes. The cache stores the *formatted* result string verbatim so a cached
//! hit is byte-identical to a live call.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use super::Database;
use crate::util::hex::hex_encode;

/// Normalize then hash a query string. Lowercases, trims, and collapses
/// internal whitespace before hashing so cosmetic differences share an entry.
pub fn hash_query(query: &str) -> String {
    let normalized: String = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    hex_encode(&digest)
}

/// A cached search row.
#[derive(Debug, Clone)]
pub struct WebSearchCacheRow {
    pub provider: String,
    pub query_hash: String,
    pub query: String,
    pub body: String,
    pub fetched_at: i64,
}

impl Database {
    /// Look up a cached search result. Returns the row even if stale; the
    /// caller decides whether to honor it based on TTL.
    pub fn web_search_cache_get(
        &self,
        provider: &str,
        query_hash: &str,
    ) -> Result<Option<WebSearchCacheRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, query_hash, query, body, fetched_at
             FROM web_search_cache WHERE provider = ?1 AND query_hash = ?2",
        )?;
        let row = stmt
            .query_row(params![provider, query_hash], |row| {
                Ok(WebSearchCacheRow {
                    provider: row.get(0)?,
                    query_hash: row.get(1)?,
                    query: row.get(2)?,
                    body: row.get(3)?,
                    fetched_at: row.get(4)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Insert or replace a cached search result.
    pub fn web_search_cache_put(
        &self,
        provider: &str,
        query_hash: &str,
        query: &str,
        body: &str,
        fetched_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO web_search_cache (provider, query_hash, query, body, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, query_hash) DO UPDATE SET
                 query = excluded.query,
                 body = excluded.body,
                 fetched_at = excluded.fetched_at",
            params![provider, query_hash, query, body, fetched_at],
        )?;
        Ok(())
    }

    /// Count of rows in the cache (for tests/diagnostics).
    pub fn web_search_cache_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM web_search_cache", [], |r| r.get(0))?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::test_db()
    }

    #[test]
    fn hash_query_normalizes_whitespace_and_case() {
        assert_eq!(hash_query("Foo Bar"), hash_query("foo  bar"));
        assert_eq!(hash_query("  hello   WORLD "), hash_query("hello world"));
    }

    #[test]
    fn hash_query_distinguishes_distinct_queries() {
        assert_ne!(hash_query("rust async"), hash_query("rust traits"));
    }

    #[test]
    fn put_then_get_returns_inserted_row() {
        let db = db();
        let h = hash_query("anything");
        db.web_search_cache_put("tavily", &h, "anything", "BODY", 100)
            .unwrap();
        let row = db.web_search_cache_get("tavily", &h).unwrap().unwrap();
        assert_eq!(row.body, "BODY");
        assert_eq!(row.fetched_at, 100);
    }

    #[test]
    fn put_replaces_existing_row_keeping_one_per_provider_query() {
        let db = db();
        let h = hash_query("q");
        db.web_search_cache_put("tavily", &h, "q", "old", 100)
            .unwrap();
        db.web_search_cache_put("tavily", &h, "q", "new", 200)
            .unwrap();
        let row = db.web_search_cache_get("tavily", &h).unwrap().unwrap();
        assert_eq!(row.body, "new");
        assert_eq!(row.fetched_at, 200);
        assert_eq!(db.web_search_cache_count().unwrap(), 1);
    }

    #[test]
    fn rows_are_partitioned_by_provider() {
        let db = db();
        let h = hash_query("q");
        db.web_search_cache_put("tavily", &h, "q", "from-tavily", 100)
            .unwrap();
        db.web_search_cache_put("brave", &h, "q", "from-brave", 100)
            .unwrap();
        let t = db.web_search_cache_get("tavily", &h).unwrap().unwrap();
        let b = db.web_search_cache_get("brave", &h).unwrap().unwrap();
        assert_eq!(t.body, "from-tavily");
        assert_eq!(b.body, "from-brave");
        assert_eq!(db.web_search_cache_count().unwrap(), 2);
    }

    #[test]
    fn get_missing_returns_none() {
        let db = db();
        assert!(db
            .web_search_cache_get("tavily", &hash_query("ghost"))
            .unwrap()
            .is_none());
    }
}
