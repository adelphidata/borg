//! Persistent cache of stdio MCP servers' advertised tool catalogs, so a daemon
//! restart can present a server's tools while its process is still starting.
//!
//! Rows are keyed by a keyed MAC of the *resolved* server config (command,
//! args, environment). Resolved env carries secrets, so the plaintext never
//! reaches the DB — only the MAC does, and keying it with a per-install secret
//! means a stolen DB can't be dictionary-attacked to recover the values.
//! Rotating a secret changes the key, which reads as a cache miss.
//!
//! Freshness is decided by the caller via `cached_at` (the `web_search_cache`
//! convention).

use anyhow::Result;
use libsql::params;
use rand::RngCore;

use super::Database;

/// `meta` key holding the per-install MAC secret.
const HMAC_KEY_META: &str = "mcp_catalog_cache_hmac_key";
/// Length of the generated MAC secret.
const HMAC_KEY_BYTES: usize = 32;

/// A cached catalog row.
#[derive(Debug, Clone)]
pub struct McpCatalogCacheRow {
    pub config_hash: String,
    pub server_name: String,
    /// The tools exactly as the server advertised them, before Borg's schema
    /// gauntlet or the manifest tool filter. Re-registering runs both again, so
    /// a cached load and a live load converge on the same registry contents.
    pub tools_json: String,
    pub server_info: Option<String>,
    pub cached_at: i64,
}

impl Database {
    /// Look up a cached catalog. Returns the row even if stale; the caller
    /// decides whether to honor it based on TTL.
    pub async fn mcp_catalog_cache_get(
        &self,
        config_hash: &str,
    ) -> Result<Option<McpCatalogCacheRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT config_hash, server_name, tools_json, server_info, cached_at
                 FROM mcp_catalog_cache WHERE config_hash = ?1",
                params!(config_hash),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(McpCatalogCacheRow {
                config_hash: row.get::<String>(0)?,
                server_name: row.get::<String>(1)?,
                tools_json: row.get::<String>(2)?,
                server_info: row.get::<Option<String>>(3)?,
                cached_at: row.get::<i64>(4)?,
            })),
            None => Ok(None),
        }
    }

    /// Store a server's catalog, replacing any previous row for that server.
    ///
    /// Keeping one row per server (rather than one per config) means a config
    /// change replaces the stale entry instead of orphaning it — otherwise
    /// every edit would leak a row that nothing can ever look up again.
    pub async fn mcp_catalog_cache_put(
        &self,
        config_hash: &str,
        server_name: &str,
        tools_json: &str,
        server_info: Option<&str>,
        cached_at: i64,
    ) -> Result<()> {
        self.mcp_catalog_cache_delete_server(server_name).await?;
        self.conn
            .execute(
                "INSERT INTO mcp_catalog_cache
                     (config_hash, server_name, tools_json, server_info, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(config_hash) DO UPDATE SET
                     server_name = excluded.server_name,
                     tools_json  = excluded.tools_json,
                     server_info = excluded.server_info,
                     cached_at   = excluded.cached_at",
                params!(config_hash, server_name, tools_json, server_info, cached_at),
            )
            .await?;
        Ok(())
    }

    /// Drop any cached catalog for `server_name`. Used when a server is removed
    /// from config or opts out of caching.
    pub async fn mcp_catalog_cache_delete_server(&self, server_name: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM mcp_catalog_cache WHERE server_name = ?1",
                params!(server_name),
            )
            .await?;
        Ok(())
    }

    /// Count of cached catalogs (for tests/diagnostics).
    pub async fn mcp_catalog_cache_count(&self) -> Result<usize> {
        let mut rows = self
            .conn
            .query("SELECT COUNT(*) FROM mcp_catalog_cache", ())
            .await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count as usize)
    }

    /// The per-install secret keying [`McpCatalogCacheRow::config_hash`],
    /// generating it on first use.
    pub async fn mcp_catalog_cache_hmac_key(&self) -> Result<Vec<u8>> {
        if let Some(existing) = self.get_meta(HMAC_KEY_META).await? {
            if let Ok(bytes) = crate::util::hex::hex_decode(&existing) {
                if bytes.len() == HMAC_KEY_BYTES {
                    return Ok(bytes);
                }
            }
            // A malformed key would silently make every lookup miss forever.
            tracing::warn!(
                target: "borg.mcp",
                event = "catalog_cache_hmac_key_malformed",
                "stored MCP catalog cache key is unusable; regenerating (cached catalogs are dropped)"
            );
        }
        let mut key = vec![0u8; HMAC_KEY_BYTES];
        rand::rng().fill_bytes(&mut key);
        self.set_meta(HMAC_KEY_META, &crate::util::hex::hex_encode(&key))
            .await?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn put_then_get_returns_inserted_row() {
        let db = db().await;
        db.mcp_catalog_cache_put("hash1", "files", "[{\"name\":\"read\"}]", Some("info"), 100)
            .await
            .unwrap();
        let row = db.mcp_catalog_cache_get("hash1").await.unwrap().unwrap();
        assert_eq!(row.server_name, "files");
        assert_eq!(row.tools_json, "[{\"name\":\"read\"}]");
        assert_eq!(row.server_info.as_deref(), Some("info"));
        assert_eq!(row.cached_at, 100);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let db = db().await;
        assert!(db.mcp_catalog_cache_get("ghost").await.unwrap().is_none());
    }

    /// A config change yields a new hash; the old row must not survive, or the
    /// cache would grow a row per edit that nothing can look up.
    #[tokio::test]
    async fn put_replaces_prior_row_for_same_server() {
        let db = db().await;
        db.mcp_catalog_cache_put("old-hash", "files", "[]", None, 100)
            .await
            .unwrap();
        db.mcp_catalog_cache_put("new-hash", "files", "[{}]", None, 200)
            .await
            .unwrap();

        assert!(db
            .mcp_catalog_cache_get("old-hash")
            .await
            .unwrap()
            .is_none());
        let row = db.mcp_catalog_cache_get("new-hash").await.unwrap().unwrap();
        assert_eq!(row.cached_at, 200);
        assert_eq!(db.mcp_catalog_cache_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn servers_do_not_evict_each_other() {
        let db = db().await;
        db.mcp_catalog_cache_put("h-files", "files", "[]", None, 100)
            .await
            .unwrap();
        db.mcp_catalog_cache_put("h-git", "git", "[]", None, 100)
            .await
            .unwrap();
        assert_eq!(db.mcp_catalog_cache_count().await.unwrap(), 2);

        db.mcp_catalog_cache_delete_server("files").await.unwrap();
        assert!(db.mcp_catalog_cache_get("h-files").await.unwrap().is_none());
        assert!(db.mcp_catalog_cache_get("h-git").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn hmac_key_is_generated_once_and_reused() {
        let db = db().await;
        let first = db.mcp_catalog_cache_hmac_key().await.unwrap();
        let second = db.mcp_catalog_cache_hmac_key().await.unwrap();
        assert_eq!(first.len(), HMAC_KEY_BYTES);
        assert_eq!(first, second, "key must be stable across calls");
        assert_ne!(first, vec![0u8; HMAC_KEY_BYTES], "key must be random");
    }

    /// A corrupted key must not wedge the cache into permanent misses.
    #[tokio::test]
    async fn malformed_hmac_key_is_regenerated() {
        let db = db().await;
        db.set_meta(HMAC_KEY_META, "not-hex").await.unwrap();
        let key = db.mcp_catalog_cache_hmac_key().await.unwrap();
        assert_eq!(key.len(), HMAC_KEY_BYTES);
        assert_eq!(
            key,
            db.mcp_catalog_cache_hmac_key().await.unwrap(),
            "regenerated key must persist"
        );
    }
}
