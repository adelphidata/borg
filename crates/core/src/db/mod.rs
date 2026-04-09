mod activity;
mod delivery;
mod memory;
mod meta;
mod migrations;
mod models;
mod multi_agent;
mod pairing;
mod plugins;
mod scripts;
mod sessions;
mod settings;
mod tasks;
mod usage;
mod vitals;
mod workflow;

#[cfg(test)]
mod tests;

pub use models::*;
pub use vitals::PendingCelebration;

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};
use tracing::instrument;

use crate::config::Config;

/// SQLite database for structured data (session metadata, scheduled tasks, task runs).
pub struct Database {
    conn: Connection,
    /// Per-installation HMAC salt for event chain keys.
    /// Derived keys prevent cross-installation HMAC forgery.
    hmac_salt: Vec<u8>,
}

impl Database {
    /// Get a reference to the underlying SQLite connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Default busy timeout for CLI usage (5 seconds).
    const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;
    /// Recommended busy timeout for gateway/concurrent workloads (30 seconds).
    pub const GATEWAY_BUSY_TIMEOUT_MS: u64 = 30000;

    /// Open (or create) the database at `~/.borg/borg.db` with default busy timeout.
    #[instrument(skip_all)]
    pub fn open() -> Result<Self> {
        Self::open_with_timeout(Self::DEFAULT_BUSY_TIMEOUT_MS)
    }

    /// Open (or create) the database with a custom busy timeout (in milliseconds).
    #[instrument(skip_all)]
    pub fn open_with_timeout(busy_timeout_ms: u64) -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("Failed to open DB at {path:?}"))?;
        Self::set_file_permissions(&path);
        Self::init_connection(conn, busy_timeout_ms)
    }

    /// Create a Database from an existing connection. Runs migrations.
    /// Used for testing with in-memory databases.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        Self::init_connection(conn, Self::DEFAULT_BUSY_TIMEOUT_MS)
    }

    /// Restrict DB file to owner-only access on Unix.
    fn set_file_permissions(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = path; // suppress unused warning on non-unix
    }

    /// Connection initialization: pragmas + migrations.
    pub(crate) fn init_connection(conn: Connection, busy_timeout_ms: u64) -> Result<Self> {
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let _: i64 = conn.query_row(
            &format!("PRAGMA busy_timeout={busy_timeout_ms}"),
            [],
            |row| row.get(0),
        )?;
        // Incremental auto-vacuum prevents unbounded DB growth from message history,
        // embeddings cache, and task runs. Requires running PRAGMA incremental_vacuum
        // periodically (done in run_migrations after schema changes).
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        let db = Self {
            conn,
            hmac_salt: Vec::new(),
        };
        db.run_migrations()?;
        // Seed all SETTING_REGISTRY defaults into the settings table.
        // INSERT OR IGNORE preserves existing user overrides; idempotent on every startup.
        db.ensure_all_settings()?;
        // Initialize per-installation HMAC salt after migrations (meta table must exist)
        let salt = db.get_or_create_hmac_salt()?;
        Ok(Self {
            conn: db.conn,
            hmac_salt: salt,
        })
    }

    /// Get or create a per-installation random salt for HMAC key derivation.
    /// Stored in the meta table so it persists across restarts but is unique per install.
    fn get_or_create_hmac_salt(&self) -> Result<Vec<u8>> {
        if let Some(hex) = self.get_meta("hmac_salt")? {
            // Decode existing salt — fail loudly on corruption
            if hex.len() == 64 {
                let bytes: Result<Vec<u8>, _> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                    .collect();
                if let Ok(b) = bytes {
                    if b.len() == 32 {
                        return Ok(b);
                    }
                }
            }
            tracing::warn!("integrity: stored hmac_salt is corrupted, regenerating");
        }
        // Generate new 32-byte random salt using OS CSPRNG
        use rand::Rng;
        let mut salt = vec![0u8; 32];
        rand::rng().fill(&mut salt[..]);
        let hex: String = salt.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        self.set_meta("hmac_salt", &hex)?;
        Ok(salt)
    }

    /// Derive an HMAC key for a specific system using the per-installation salt.
    /// Derive a domain-specific HMAC key from the per-installation salt.
    /// Returns sensitive key material — callers should not log or persist the result.
    /// Uses HMAC-SHA256(key=salt, data=domain) as a simple KDF.
    #[allow(clippy::expect_used)]
    pub fn derive_hmac_key(&self, domain: &[u8]) -> Vec<u8> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac =
            HmacSha256::new_from_slice(&self.hmac_salt).expect("HMAC accepts any key size");
        mac.update(domain);
        mac.finalize().into_bytes().to_vec()
    }

    fn db_path() -> Result<PathBuf> {
        Config::db_path()
    }

    /// Current schema version. Bump this when adding new migrations.
    const CURRENT_VERSION: u32 = 33;

    /// Check if a column exists on a table via `PRAGMA table_info`.
    /// Safer than catching ALTER TABLE errors by string matching.
    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = match conn.prepare(&format!("PRAGMA table_info(\"{table}\")")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let names: Vec<String> = match stmt.query_map([], |row| row.get::<_, String>(1)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return false,
        };
        names.iter().any(|name| name == column)
    }

    fn run_migrations(&self) -> Result<()> {
        // Ensure meta table exists for version tracking (outside transaction
        // so schema_version() can read it)
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        let current = self.schema_version()?;
        if current >= Self::CURRENT_VERSION {
            return Ok(());
        }

        // Run all pending migrations in a single transaction for atomicity.
        // SQLite supports transactional DDL (CREATE TABLE, ALTER TABLE).
        // unchecked_transaction avoids rusqlite's borrow-check restriction
        // while still giving us automatic ROLLBACK on drop if not committed.
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("Failed to begin migration transaction")?;

        const MIGRATIONS: &[fn(&Database) -> Result<()>] = &[
            Database::migrate_v1,
            Database::migrate_v2,
            Database::migrate_v3,
            Database::migrate_v4,
            Database::migrate_v5,
            Database::migrate_v6,
            Database::migrate_v7,
            Database::migrate_v8,
            Database::migrate_v9,
            Database::migrate_v10,
            Database::migrate_v11,
            Database::migrate_v12,
            Database::migrate_v13,
            Database::migrate_v14,
            Database::migrate_v15,
            Database::migrate_v16,
            Database::migrate_v17,
            Database::migrate_v18,
            Database::migrate_v19,
            Database::migrate_v20,
            Database::migrate_v21,
            Database::migrate_v22,
            Database::migrate_v23,
            Database::migrate_v24,
            Database::migrate_v25,
            Database::migrate_v26,
            Database::migrate_v27,
            Database::migrate_v28,
            Database::migrate_v29,
            Database::migrate_v30,
            Database::migrate_v31,
            Database::migrate_v32,
            Database::migrate_v33,
        ];
        // Compile-time guard: adding a migration without updating CURRENT_VERSION (or vice versa)
        // will fail the build.
        const _: () = assert!(
            MIGRATIONS.len() == Database::CURRENT_VERSION as usize,
            "MIGRATIONS array length must match CURRENT_VERSION"
        );

        for (i, migrate_fn) in MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as u32;
            if current < version {
                migrate_fn(self)?;
            }
        }

        self.set_meta("schema_version", &Self::CURRENT_VERSION.to_string())?;
        tx.commit().context("Failed to commit migrations")?;

        // Run incremental vacuum after migrations to reclaim freed pages.
        self.conn.execute_batch("PRAGMA incremental_vacuum;")?;

        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        match self.get_meta("schema_version")? {
            Some(v) => match v.parse() {
                Ok(n) => Ok(n),
                Err(_) => {
                    tracing::warn!("Corrupted schema_version '{v}', treating as 0");
                    Ok(0)
                }
            },
            None => {
                // Check if tables already exist (pre-versioning database)
                let mut stmt = self.conn.prepare(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sessions'",
                )?;
                let count: i64 = stmt.query_row([], |row| row.get(0))?;
                if count > 0 {
                    Ok(1) // Legacy DB with original tables
                } else {
                    Ok(0) // Fresh database
                }
            }
        }
    }
}

#[cfg(test)]
impl Database {
    /// Create an in-memory database for tests. Runs all migrations.
    pub fn test_db() -> Self {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        Self::from_connection(conn).expect("init test db")
    }
}
