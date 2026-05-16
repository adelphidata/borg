mod activity;
mod audit_chain;
mod audit_writer;
mod delivery;
mod device_pairing;
pub mod doctor_squelch;
pub(crate) mod encryption;
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

// Gated on `phase9-tests-disabled` because the split-out `db::tests::*`
// submodules still call the pre-turso sync helper signatures (`test_db()`
// without `.await`, `db.conn().execute(..., [], ...)`-style rusqlite calls).
// Tracked work, not a regression — matches the precedent set by the 17
// other modules behind this feature. The shared async helper at
// `db::tests::mod` is already turso-shaped; only the per-topic test bodies
// remain to port.
#[cfg(all(test, feature = "phase9-tests-disabled"))]
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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tracing::instrument;
use turso::{Builder, Connection, EncryptionOpts};

use crate::config::Config;

/// SQLite-compatible database (Turso 0.6) for structured borg state.
///
/// Holds an internally-`Arc`'d `turso::Connection`, which is `Send + Sync +
/// Clone`. We deliberately do NOT wrap it in a further `Arc` — cloning the
/// connection is already shallow, and an outer `Arc` would just hide the
/// fact that turso owns its own pool.
pub struct Database {
    conn: Connection,
    /// Per-installation HMAC salt for event chain keys.
    /// Derived keys prevent cross-installation HMAC forgery.
    hmac_salt: Vec<u8>,
    /// Cached Ed25519 signing key + matching `device_keys.id`. Populated on
    /// first call to `record_evolution_event`; survives the process lifetime.
    /// `None` until first use; loading from the OS keychain is gated until
    /// the evolution write path actually fires. `std::sync::Mutex` rather
    /// than `RefCell` because the database now lives in async contexts
    /// where `!Send` interior mutability would block its use across `.await`.
    evolution_signing_key: Mutex<Option<(ed25519_dalek::SigningKey, i64)>>,
    /// Optional override keystore. Production code uses
    /// [`crate::evolution::keychain::default_keystore`]; tests inject an
    /// in-memory store so they don't touch the real OS keychain.
    evolution_keystore_override: Mutex<Option<Box<dyn crate::evolution::keychain::KeyStore>>>,
    /// True when this handle was opened via [`open_readonly`]. A debug-only
    /// guard catches accidental writes through a read-only handle; Turso
    /// itself has no `SQLITE_OPEN_READ_ONLY` equivalent, so the daemon
    /// remains the single writer by convention.
    read_only: bool,
}

impl Database {
    /// Get a reference to the underlying connection. Cheap to clone if the
    /// caller needs an owned handle for an async task — turso's `Connection`
    /// is internally `Arc`'d.
    pub fn conn(&self) -> &Connection {
        debug_assert!(
            !self.read_only,
            "mutating call through a read-only Database handle; daemon is the single writer"
        );
        &self.conn
    }

    /// True if this handle was opened read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Default busy timeout for CLI usage (5 seconds).
    const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;
    /// Recommended busy timeout for gateway/concurrent workloads (30 seconds).
    pub const GATEWAY_BUSY_TIMEOUT_MS: u64 = crate::constants::GATEWAY_BUSY_TIMEOUT_MS;

    /// Open (or create) the database at `~/.borg/borg.db` with default busy
    /// timeout. Opens **unencrypted** — Phase 4 wires the KEK-derived hex key
    /// through the daemon bootstrap path; until then production callers
    /// must explicitly use [`open_encrypted`](Self::open_encrypted).
    #[instrument(skip_all)]
    pub async fn open() -> Result<Self> {
        Self::open_with_timeout(Self::DEFAULT_BUSY_TIMEOUT_MS).await
    }

    /// Open (or create) the database with a custom busy timeout (in milliseconds).
    #[instrument(skip_all)]
    pub async fn open_with_timeout(busy_timeout_ms: u64) -> Result<Self> {
        let path = Self::db_path()?;
        Self::open_at_inner(&path, busy_timeout_ms, None, false).await
    }

    /// Open (or create) the database with a KEK-derived hex encryption key.
    /// `hex_key` must be 64 ASCII hex chars (32 bytes of AES-256 material) —
    /// produced by [`encryption::derive_db_hexkey`].
    #[instrument(skip_all)]
    pub async fn open_encrypted(hex_key: &str) -> Result<Self> {
        let path = Self::db_path()?;
        Self::open_at_inner(&path, Self::DEFAULT_BUSY_TIMEOUT_MS, Some(hex_key), false).await
    }

    /// Create a Database from an existing turso connection. Runs migrations.
    /// Used by [`test_db`](Self::test_db) and any caller that already owns a
    /// connection (e.g. backup inspection over a custom path).
    pub async fn from_connection(conn: Connection) -> Result<Self> {
        Self::init_connection(conn, Self::DEFAULT_BUSY_TIMEOUT_MS, false).await
    }

    /// Open (or create) a database at an arbitrary filesystem path. Runs
    /// migrations. Unencrypted — used by [`crate::backup`] to inspect raw
    /// DB files inside extracted tarballs.
    #[instrument(skip_all)]
    pub async fn open_at(path: &Path) -> Result<Self> {
        Self::open_at_inner(path, Self::DEFAULT_BUSY_TIMEOUT_MS, None, false).await
    }

    /// Open the database read-only. Skips writes — intended for read-only
    /// consumers (e.g. the CLI reading config/settings) that must not race
    /// the daemon's writer. Turso 0.6 has no `SQLITE_OPEN_READ_ONLY`
    /// equivalent, so the read-only contract is enforced by:
    ///   1. Skipping migrations + settings seeding + HMAC-salt init (all
    ///      writes that the daemon already performed).
    ///   2. A `read_only` flag carried on the handle that fires a
    ///      `debug_assert!` from [`conn()`](Self::conn).
    /// CLI processes never execute mutating SQL through this handle today;
    /// the assert exists to catch a future regression early.
    #[instrument(skip_all)]
    pub async fn open_readonly() -> Result<Self> {
        let path = Self::db_path()?;
        // Turso's `Builder::new_local` creates the file if missing, which
        // breaks `load_from_db`'s expectation that a missing DB fails here
        // and falls through to the writer path (which runs migrations).
        // Mirror the legacy SQLITE_OPEN_READ_ONLY behaviour by erroring out
        // when the file doesn't exist yet.
        if !path.exists() {
            anyhow::bail!(
                "no database at {} — daemon hasn't created it yet",
                path.display()
            );
        }
        let conn = Self::build_connection(&path, None).await?;
        conn.busy_timeout(Duration::from_millis(Self::DEFAULT_BUSY_TIMEOUT_MS))
            .context("set busy_timeout on read-only handle")?;
        Ok(Self {
            conn,
            hmac_salt: Vec::new(),
            evolution_signing_key: Mutex::new(None),
            evolution_keystore_override: Mutex::new(None),
            read_only: true,
        })
    }

    /// Build a fresh `turso::Connection`. Always enables the experimental
    /// flags borg depends on (encryption, vacuum, custom index methods);
    /// `hex_key` opts into AES-256-GCM at rest.
    /// True if `path` refers to an in-memory turso database. Used to skip
    /// journal-mode pragmas that turso 0.6 errors on for in-memory connections
    /// (WAL has no meaning when there's no file to persist).
    fn is_memory_path(path: &Path) -> bool {
        matches!(path.to_str(), Some(":memory:"))
    }

    async fn build_connection(path: &Path, hex_key: Option<&str>) -> Result<Connection> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create_dir_all({})", parent.display()))?;
            }
        }
        let path_str = path
            .to_str()
            .with_context(|| format!("non-UTF-8 db path: {}", path.display()))?;

        let mut builder = Builder::new_local(path_str)
            .experimental_encryption(true)
            .experimental_vacuum(true)
            .experimental_index_method(true);
        if let Some(hex) = hex_key {
            builder = builder.with_encryption(EncryptionOpts {
                cipher: "aes256gcm".into(),
                hexkey: hex.into(),
            });
        }

        let db = builder
            .build()
            .await
            .with_context(|| format!("turso build at {}", path.display()))?;
        Self::set_file_permissions(path);
        db.connect()
            .with_context(|| format!("turso connect at {}", path.display()))
    }

    async fn open_at_inner(
        path: &Path,
        busy_timeout_ms: u64,
        hex_key: Option<&str>,
        read_only: bool,
    ) -> Result<Self> {
        let conn = Self::build_connection(path, hex_key).await?;
        Self::init_connection_inner(conn, busy_timeout_ms, read_only, Self::is_memory_path(path))
            .await
    }

    /// Restrict DB file to owner-only access on Unix.
    fn set_file_permissions(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: an in-memory DB or freshly-created file in a
            // not-yet-flushed state is a non-error.
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("failed to chmod 0600 db file {}: {e}", path.display());
            }
        }
        let _ = path; // suppress unused warning on non-unix
    }

    /// Connection initialization for callers that already own a Connection
    /// and don't know (or don't care about) its backing path. Treats the
    /// connection as file-backed — pragmas are applied unconditionally.
    pub(crate) async fn init_connection(
        conn: Connection,
        busy_timeout_ms: u64,
        read_only: bool,
    ) -> Result<Self> {
        Self::init_connection_inner(conn, busy_timeout_ms, read_only, false).await
    }

    /// Connection initialization: pragmas + migrations. `in_memory=true`
    /// skips journal-mode pragmas that turso 0.6 errors on for `:memory:`
    /// connections (WAL is meaningless without a backing file).
    async fn init_connection_inner(
        conn: Connection,
        busy_timeout_ms: u64,
        read_only: bool,
        in_memory: bool,
    ) -> Result<Self> {
        // Serialize init across the process. Turso (like sqlite) has a
        // cold-start race where multiple connections opening a fresh DB
        // file all try to switch to WAL and one loses the writer lock.
        // borgd hits this on every start (daemon.rs, budget_tick,
        // config_watcher, PKI bootstrap each open their own handle).
        // Holding INIT_LOCK for the ~tens-of-ms init window is free in
        // production and deterministically eliminates the race.
        static INIT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _init_guard = INIT_LOCK.lock().await;

        // busy_timeout first: subsequent pragma writes take a writer lock
        // and would otherwise return SQLITE_BUSY immediately when another
        // connection is mid-init on the same file.
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))
            .context("set busy_timeout")?;
        if !in_memory {
            // Best-effort: turso 0.6 already uses WAL natively (it's the
            // default underlying journal mode and not really configurable
            // via PRAGMA at the parser level for all paths). Older daemon
            // startup paths still set it explicitly so a future migration
            // back to rusqlite-style sqlite would behave identically; if
            // turso rejects the pragma, log and continue rather than abort
            // — WAL is on regardless.
            if let Err(e) = conn.execute_batch("PRAGMA journal_mode=WAL;").await {
                tracing::debug!(
                    error = %e,
                    "PRAGMA journal_mode=WAL not accepted (turso is WAL by default); continuing",
                );
            }
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL;")
            .await
            .context("set synchronous=NORMAL")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .await
            .context("set foreign_keys=ON")?;
        // NOTE: auto_vacuum / incremental_vacuum are unsupported in Turso
        // 0.6 — the equivalent reclamation lives in the maintenance
        // sweep's conditional full VACUUM.
        let db = Self {
            conn,
            hmac_salt: Vec::new(),
            evolution_signing_key: Mutex::new(None),
            evolution_keystore_override: Mutex::new(None),
            read_only,
        };
        if !read_only {
            db.run_migrations().await?;
            // INSERT OR IGNORE preserves existing user overrides; idempotent.
            db.ensure_all_settings().await?;
        }
        let salt = if read_only {
            Vec::new()
        } else {
            db.get_or_create_hmac_salt().await?
        };
        Ok(Self {
            conn: db.conn,
            hmac_salt: salt,
            evolution_signing_key: Mutex::new(None),
            evolution_keystore_override: Mutex::new(None),
            read_only,
        })
    }

    /// Inject a keystore for the evolution signing path. Intended for
    /// tests across crates (cortex/gateway/borgd/interface), where the
    /// real OS keychain must not be touched.
    pub fn set_evolution_keystore(&self, store: Box<dyn crate::evolution::keychain::KeyStore>) {
        let mut guard = self
            .evolution_keystore_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(store);
        let mut key_guard = self
            .evolution_signing_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *key_guard = None;
    }

    /// Load (or generate + persist) the Ed25519 signing key used to sign new
    /// evolution events. Cached for the process lifetime. The matching public
    /// key is mirrored into `device_keys`; the returned i64 is its row id and
    /// is what `evolution_events.pubkey_id` references.
    pub(crate) async fn evolution_signing_key(&self) -> Result<(ed25519_dalek::SigningKey, i64)> {
        {
            let guard = self
                .evolution_signing_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ref cached) = *guard {
                return Ok((cached.0.clone(), cached.1));
            }
        }
        // Resolve a keystore: test override or platform default.
        let (key, _created) = {
            let guard = self
                .evolution_keystore_override
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let pubkey_blob = pubkey_bytes.to_vec();
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM device_keys WHERE public_key = ?1 LIMIT 1",
                turso::params![pubkey_blob.clone()],
            )
            .await?;
        let existing: Option<i64> = match rows.next().await? {
            Some(row) => Some(row.get::<i64>(0)?),
            None => None,
        };
        let pubkey_id = match existing {
            Some(id) => id,
            None => {
                let now = chrono::Utc::now().timestamp();
                self.conn
                    .execute(
                        "INSERT INTO device_keys (public_key, created_at) VALUES (?1, ?2)",
                        turso::params![pubkey_blob, now],
                    )
                    .await?;
                self.conn.last_insert_rowid()
            }
        };

        let mut guard = self
            .evolution_signing_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some((key.clone(), pubkey_id));
        Ok((key, pubkey_id))
    }

    /// Get or create a per-installation random salt for HMAC key derivation.
    /// Stored in the meta table so it persists across restarts but is unique per install.
    async fn get_or_create_hmac_salt(&self) -> Result<Vec<u8>> {
        if let Some(hex) = self.get_meta("hmac_salt").await? {
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
        use rand::Rng;
        let mut salt = vec![0u8; 32];
        rand::rng().fill(&mut salt[..]);
        let hex: String = salt.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        self.set_meta("hmac_salt", &hex).await?;
        Ok(salt)
    }

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
    const CURRENT_VERSION: u32 = 2;

    async fn run_migrations(&self) -> Result<()> {
        // Ensure meta table exists for version tracking (outside the writer
        // transaction so schema_version() can read it).
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .await?;

        let current = self.schema_version().await?;
        if current >= Self::CURRENT_VERSION {
            return Ok(());
        }

        // Acquire the writer lock explicitly. Turso's `Transaction` API is
        // built around `&mut Connection`, but our submodules hold the
        // connection behind `&self`; opening the writer via raw BEGIN
        // IMMEDIATE + an explicit COMMIT/ROLLBACK guard sidesteps that
        // borrow restriction while keeping the same atomicity guarantee
        // sqlite's transactional DDL gives us.
        self.conn
            .execute_batch("BEGIN IMMEDIATE;")
            .await
            .context("Failed to begin migration transaction")?;
        let migration_result = self.run_migrations_inner().await;
        match &migration_result {
            Ok(()) => {
                self.conn
                    .execute_batch("COMMIT;")
                    .await
                    .context("Failed to commit migrations")?;
            }
            Err(e) => {
                if let Err(rb) = self.conn.execute_batch("ROLLBACK;").await {
                    tracing::warn!("rollback after failed migration failed: {rb} (original: {e})");
                }
            }
        }
        migration_result
    }

    /// Body of the migration writer transaction; caller owns BEGIN/COMMIT.
    async fn run_migrations_inner(&self) -> Result<()> {
        // Re-check inside the writer lock: another process/connection may
        // have committed migrations while we were waiting on BEGIN
        // IMMEDIATE. Without this double-check, two concurrent fresh-DB
        // openers both see version=0 outside the lock, both eventually
        // acquire the writer in turn, and the second runs `CREATE TABLE …`
        // against tables the first just created.
        let current = self.schema_version().await?;
        if current >= Self::CURRENT_VERSION {
            return Ok(());
        }

        // Async fn pointers don't compose into a slice literal the way sync
        // ones did; the migration set is tiny so a direct dispatch keeps the
        // CURRENT_VERSION→migrate_vN coupling explicit.
        if current < 1 {
            self.migrate_v1().await?;
        }
        if current < 2 {
            self.migrate_v2().await?;
        }
        const _: () = assert!(
            Database::CURRENT_VERSION == 2,
            "add the new migrate_vN call above and bump this constant",
        );

        self.set_meta("schema_version", &Self::CURRENT_VERSION.to_string())
            .await?;
        Ok(())
    }

    pub async fn schema_version(&self) -> Result<u32> {
        match self.get_meta("schema_version").await? {
            Some(v) => match v.parse() {
                Ok(n) => Ok(n),
                Err(_) => {
                    tracing::warn!("Corrupted schema_version '{v}', treating as 0");
                    Ok(0)
                }
            },
            None => {
                // Check if tables already exist (pre-versioning database)
                let mut rows = self
                    .conn
                    .query(
                        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sessions'",
                        (),
                    )
                    .await?;
                let count: i64 = match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                };
                if count > 0 {
                    Ok(1) // Legacy DB with original tables
                } else {
                    Ok(0) // Fresh database
                }
            }
        }
    }
}

impl Database {
    /// Create an in-memory database for tests. Runs all migrations and
    /// installs an in-memory keystore so the evolution signing path never
    /// touches the real OS keychain during tests. Skips `with_encryption()`
    /// — in-memory DBs don't need a real KEK round-trip.
    ///
    /// Visible across crates so downstream test suites (gateway, borgd,
    /// interface) can share the same fixture without re-implementing it.
    #[allow(clippy::expect_used)]
    pub async fn test_db() -> Self {
        let conn = Self::build_connection(Path::new(":memory:"), None)
            .await
            .expect("open in-memory db");
        let db = Self::init_connection_inner(conn, Self::DEFAULT_BUSY_TIMEOUT_MS, false, true)
            .await
            .expect("init test db");
        db.set_evolution_keystore(Box::new(crate::evolution::keychain::InMemoryKeyStore::new()));
        db
    }
}
