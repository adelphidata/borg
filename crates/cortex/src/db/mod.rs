mod activity;
mod audit_chain;
mod audit_writer;
mod delivery;
mod device_pairing;
pub mod doctor_squelch;
mod goals;
mod issue_state;
mod maintenance;
mod memory;
mod meta;
mod migrations;
mod models;
mod multi_agent;
mod pairing;
mod pki;
mod plugins;
mod redaction_audit;
mod scripts;
mod sessions;
mod settings;
mod skill_audit;
mod tasks;
mod usage;
mod vitals;
pub mod wal;
pub mod web_search_cache;

#[cfg(test)]
mod tests;

pub use audit_chain::{compute_row_hash, AuditChainOutcome, AuditChainRow, GENESIS_PREV_HASH};
pub use audit_writer::{audit_events, install_global_audit, try_emit_global, AuditWriter};
pub use device_pairing::{hash_pairing_code, ConsumeOutcome};
pub use goals::{GoalAccountingOutcome, GoalUpdate};
pub use issue_state::IssueStateRow;
pub use models::*;
pub use pki::{InternalCertRow, PairedDeviceRow};
pub use redaction_audit::RedactionAuditRow;
pub use sessions::MessageFtsHit;
pub use skill_audit::{SkillAuditOutcome, SkillAuditRow};
pub use vitals::PendingCelebration;
pub use web_search_cache::{hash_query as hash_search_query, WebSearchCacheRow};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::{Path, PathBuf};
use tracing::instrument;

use crate::config::Config;

/// SQLite database for structured data (session metadata, scheduled tasks, task runs).
pub struct Database {
    conn: Connection,
    /// Per-installation HMAC salt for event chain keys.
    /// Derived keys prevent cross-installation HMAC forgery.
    hmac_salt: Vec<u8>,
    /// Cached Ed25519 signing key + matching `device_keys.id`. Populated on
    /// first call to `record_evolution_event`; survives the process lifetime.
    /// `None` until first use; loading from the OS keychain is gated until
    /// the evolution write path actually fires.
    evolution_signing_key: std::cell::RefCell<Option<(ed25519_dalek::SigningKey, i64)>>,
    /// Optional override keystore. Production code uses
    /// [`crate::evolution::keychain::default_keystore`]; tests inject an
    /// in-memory store so they don't touch the real OS keychain.
    evolution_keystore_override:
        std::cell::RefCell<Option<Box<dyn crate::evolution::keychain::KeyStore>>>,
}

impl Database {
    /// Get a reference to the underlying SQLite connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Default busy timeout for CLI usage (5 seconds).
    const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;
    /// Recommended busy timeout for gateway/concurrent workloads (30 seconds).
    pub const GATEWAY_BUSY_TIMEOUT_MS: u64 = crate::constants::GATEWAY_BUSY_TIMEOUT_MS;

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

    /// Open (or create) a database at an arbitrary filesystem path. Runs
    /// migrations. Used by [`crate::backup`] to inspect SQLite files
    /// inside extracted tarballs without going through `Config::db_path`.
    #[instrument(skip_all)]
    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn =
            Connection::open(path).with_context(|| format!("Failed to open DB at {path:?}"))?;
        Self::set_file_permissions(path);
        Self::init_connection(conn, Self::DEFAULT_BUSY_TIMEOUT_MS)
    }

    /// Open the database read-only. Skips migrations and writes — intended
    /// for read-only consumers (e.g. the cli reading config/settings). The
    /// single-writer invariant means only `borgd` may use [`open()`](Self::open)
    /// or [`open_with_timeout()`](Self::open_with_timeout) for writes; CLI
    /// processes that race the daemon's writer corrupt state.
    ///
    /// The connection is opened with `SQLITE_OPEN_READ_ONLY |
    /// SQLITE_OPEN_NO_MUTEX`; any attempt to mutate via this handle returns
    /// `SQLITE_READONLY` from rusqlite.
    #[instrument(skip_all)]
    pub fn open_readonly() -> Result<Self> {
        let path = Self::db_path()?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&path, flags)
            .with_context(|| format!("Failed to open DB read-only at {path:?}"))?;
        let _: i64 = conn.query_row(
            &format!("PRAGMA busy_timeout={}", Self::DEFAULT_BUSY_TIMEOUT_MS),
            [],
            |row| row.get(0),
        )?;
        // Read-only handle: no migrations, no settings seeding, no HMAC salt
        // initialization (those are writes — they belong to the daemon).
        // `hmac_salt` stays empty; consumers that need it must go through
        // the daemon over gRPC.
        Ok(Self {
            conn,
            hmac_salt: Vec::new(),
            evolution_signing_key: std::cell::RefCell::new(None),
            evolution_keystore_override: std::cell::RefCell::new(None),
        })
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
        // Serialize init across the process. SQLite's `busy_timeout` doesn't
        // reliably cover the cold-start race where several connections all
        // try to switch a fresh DB file from rollback-journal to WAL: the
        // `PRAGMA journal_mode=WAL` path takes a brief exclusive lock and
        // returns SQLITE_BUSY without retrying. Borgd hits this on every
        // start (daemon.rs, budget_tick, config_watcher, PKI bootstrap each
        // open their own handle in parallel). Holding `INIT_LOCK` for the
        // ~tens-of-ms init window costs nothing in production but eliminates
        // the race deterministically; the regression is covered by
        // `tests/db_concurrent_open.rs`. Poison recovery: another thread
        // panicking mid-init shouldn't permanently brick the lock — its
        // partial state is on a different connection and doesn't affect us.
        static INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _init_guard = INIT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // busy_timeout must be set first: subsequent pragma writes
        // (`journal_mode=WAL`, `auto_vacuum`) take a writer lock and would
        // otherwise return SQLITE_BUSY immediately when another connection is
        // mid-init on the same file.
        let _: i64 = conn.query_row(
            &format!("PRAGMA busy_timeout={busy_timeout_ms}"),
            [],
            |row| row.get(0),
        )?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        // Incremental auto-vacuum prevents unbounded DB growth from message history,
        // embeddings cache, and task runs. Requires running PRAGMA incremental_vacuum
        // periodically (done in run_migrations after schema changes).
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        let db = Self {
            conn,
            hmac_salt: Vec::new(),
            evolution_signing_key: std::cell::RefCell::new(None),
            evolution_keystore_override: std::cell::RefCell::new(None),
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
            evolution_signing_key: std::cell::RefCell::new(None),
            evolution_keystore_override: std::cell::RefCell::new(None),
        })
    }

    /// Inject a keystore for the evolution signing path. Test-only: production
    /// code uses [`crate::evolution::keychain::default_keystore`] resolved
    /// lazily on first signing-event write. Once set, replaces any previously
    /// cached signing key so the next write rebuilds from the override.
    #[cfg(test)]
    pub fn set_evolution_keystore(&self, store: Box<dyn crate::evolution::keychain::KeyStore>) {
        *self.evolution_keystore_override.borrow_mut() = Some(store);
        *self.evolution_signing_key.borrow_mut() = None;
    }

    /// Load (or generate + persist) the Ed25519 signing key used to sign new
    /// evolution events. Cached for the process lifetime. The matching public
    /// key is mirrored into `device_keys`; the returned i64 is its row id and
    /// is what `evolution_events.pubkey_id` references.
    pub(crate) fn evolution_signing_key(&self) -> Result<(ed25519_dalek::SigningKey, i64)> {
        if let Some(ref cached) = *self.evolution_signing_key.borrow() {
            return Ok((cached.0.clone(), cached.1));
        }
        // Resolve a keystore: test override or platform default.
        let (key, _created) = {
            let guard = self.evolution_keystore_override.borrow();
            match guard.as_ref() {
                Some(store) => {
                    crate::evolution::keychain::load_or_create_signing_key(store.as_ref())?
                }
                None => crate::evolution::keychain::load_or_create_signing_key(
                    crate::evolution::keychain::default_keystore().as_ref(),
                )?,
            }
        };
        let pubkey_bytes = key.verifying_key().to_bytes();

        // Find or insert the matching device_keys row. Reuse on subsequent runs
        // by matching public_key bytes — keeps the table compact.
        let pubkey_blob = pubkey_bytes.to_vec();
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM device_keys WHERE public_key = ?1 LIMIT 1",
                rusqlite::params![pubkey_blob],
                |row| row.get(0),
            )
            .ok();
        let pubkey_id = match existing {
            Some(id) => id,
            None => {
                let now = chrono::Utc::now().timestamp();
                self.conn.execute(
                    "INSERT INTO device_keys (public_key, created_at) VALUES (?1, ?2)",
                    rusqlite::params![pubkey_blob, now],
                )?;
                self.conn.last_insert_rowid()
            }
        };

        *self.evolution_signing_key.borrow_mut() = Some((key.clone(), pubkey_id));
        Ok((key, pubkey_id))
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
    const CURRENT_VERSION: u32 = 1;

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

        // Re-check inside the writer lock: another process/connection may have
        // committed migrations while we were waiting on BEGIN IMMEDIATE. Without
        // this double-check, two concurrent fresh-DB openers both see version=0
        // outside the lock, both eventually acquire the writer in turn, and the
        // second runs `CREATE TABLE …` against tables the first just created.
        let current = self.schema_version()?;
        if current >= Self::CURRENT_VERSION {
            return Ok(());
        }

        const MIGRATIONS: &[fn(&Database) -> Result<()>] = &[Database::migrate_v1];
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

    pub fn schema_version(&self) -> Result<u32> {
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
    /// Create an in-memory database for tests. Runs all migrations and
    /// installs an in-memory keystore so the evolution signing path never
    /// touches the real OS keychain during tests.
    pub fn test_db() -> Self {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let db = Self::from_connection(conn).expect("init test db");
        db.set_evolution_keystore(Box::new(crate::evolution::keychain::InMemoryKeyStore::new()));
        db
    }
}
