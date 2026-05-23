//! Per-provider credential pool with exhaustion tracking.
//!
//! Replaces the legacy FIFO key rotation. Each `Provider` owns a pool of
//! `PooledCredential` entries. The pool selects keys by strategy
//! (fill_first / round_robin / least_used / random), tracks per-credential
//! cooldown TTL on 402/429, soft-deletes on 401/403, and persists state to
//! SQLite so daemon restarts respect in-flight cooldowns.
//!
//! See `docs/llm-resilience.md` for the operator-facing surface.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use libsql::params;
use rand::prelude::IndexedRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::db::Database;
use crate::llm::llm_error::FailoverReason;
use crate::llm::provider::Provider;
use crate::llm::rate_limit_headers::RateLimitSnapshot;

/// Hard ceiling for any cooldown duration — defends against absurd provider
/// hints like `retry_after: 999999999` (yes, it happens).
const COOLDOWN_MAX_SECS: u64 = 24 * 60 * 60;

/// Truncate persisted error messages to keep `borg auth list` and daemon.log bounded.
const ERROR_MESSAGE_MAX_LEN: usize = 500;

// ── Strategy / status / source ────────────────────────────────────────────

/// How `pick()` chooses the next credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStrategy {
    /// Use the lowest-priority available key; only advance on failure.
    #[default]
    FillFirst,
    /// Rotate to the next key after each pick.
    RoundRobin,
    /// Pick the key with the lowest `use_count`.
    LeastUsed,
    /// Uniform random across available keys.
    Random,
}

impl std::str::FromStr for CredentialStrategy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fill_first" | "fillfirst" => Ok(Self::FillFirst),
            "round_robin" | "roundrobin" => Ok(Self::RoundRobin),
            "least_used" | "leastused" => Ok(Self::LeastUsed),
            "random" => Ok(Self::Random),
            other => Err(anyhow!(
                "Unknown credential strategy '{other}'. Valid: fill_first, round_robin, least_used, random"
            )),
        }
    }
}

impl std::fmt::Display for CredentialStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FillFirst => "fill_first",
            Self::RoundRobin => "round_robin",
            Self::LeastUsed => "least_used",
            Self::Random => "random",
        })
    }
}

/// Per-credential health state. Removed entries are kept as tombstones for
/// `borg auth list` history but are excluded from `pick()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    #[default]
    Ok,
    Exhausted,
}

impl CredentialStatus {
    fn as_db(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Exhausted => "exhausted",
        }
    }
    fn from_db(s: &str) -> Self {
        match s {
            "exhausted" => Self::Exhausted,
            _ => Self::Ok,
        }
    }
}

/// Where the credential came from. Drives the suppression model — operators
/// running `borg auth remove openrouter 2` write a row keyed by source so
/// re-seeding from env/config doesn't undo the removal on next start.
///
/// `OAuth` and `Custom` capture the provenance for accounts discovered on
/// disk (Claude Code, OpenAI OAuth, etc.) and user-defined OpenAI-compatible
/// endpoints. Discovery itself lives in `crates/cortex/src/llm/credential_sources/`;
/// this enum only records the source so suppression and `borg auth list` can
/// distinguish "operator added a key" from "discovered on disk".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource {
    /// Explicitly added via `borg auth add` or a freeform config entry.
    Manual,
    /// Seeded from an environment variable (e.g. `OPENROUTER_API_KEY`).
    Env { var: String },
    /// Seeded from a named config block (`llm.api_keys[i]` etc.).
    Config { name: String },
    /// Seeded from an OAuth-capable on-disk credential store. `name` is the
    /// source identifier — `"claude_code" | "codex" | "qwen_cli"` today;
    /// the field is `name` (not `kind`) to avoid colliding with serde's
    /// `tag = "kind"` discriminator on this enum.
    OAuth { name: String },
    /// User-defined OpenAI-compatible endpoint. `name` is the operator's label
    /// (also the suppression key); `base_url` overrides the provider's default
    /// for this credential's requests.
    Custom { name: String, base_url: String },
}

impl CredentialSource {
    fn kind_db(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Env { .. } => "env",
            Self::Config { .. } => "config",
            Self::OAuth { .. } => "oauth",
            Self::Custom { .. } => "custom",
        }
    }
    fn value_db(&self) -> String {
        match self {
            Self::Manual => String::new(),
            Self::Env { var } => var.clone(),
            Self::Config { name } => name.clone(),
            Self::OAuth { name } => name.clone(),
            // Suppression keys on `name`, not `base_url` — base_url can change
            // (port flip on a self-hosted endpoint) but the credential's
            // identity is the operator-chosen name.
            Self::Custom { name, .. } => name.clone(),
        }
    }
    #[allow(dead_code)]
    fn from_db(kind: &str, value: &str) -> Self {
        match kind {
            "env" => Self::Env { var: value.into() },
            "config" => Self::Config { name: value.into() },
            "oauth" => Self::OAuth { name: value.into() },
            "custom" => Self::Custom {
                name: value.into(),
                // base_url is not round-trippable from the suppression value
                // alone; suppression entries don't need it (the lookup key is
                // (provider, kind="custom", name)). Empty here is fine.
                base_url: String::new(),
            },
            _ => Self::Manual,
        }
    }
}

/// Authentication style for the credential — drives header construction in
/// `Provider::build_headers` and OAuth-token-refresh scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Static bearer-shaped secret (API key). No refresh.
    #[default]
    ApiKey,
    /// OAuth access token. May have a refresh_token + expires_at.
    OAuth,
}

// ── PooledCredential ──────────────────────────────────────────────────────

/// One credential in the pool. The plaintext `key` is held in memory only;
/// the DB row is keyed by `fingerprint` (16-hex BLAKE3 truncation).
///
/// `Debug` is hand-implemented to redact the plaintext `key` — accidentally
/// `{:?}`-ing a credential (directly or transitively through `PoolInner`)
/// must not leak the secret.
#[derive(Clone)]
pub struct PooledCredential {
    pub id: String,
    pub label: String,
    pub priority: u32,
    pub source: CredentialSource,
    /// Bearer-shaped secret. For `auth_type=ApiKey` this is the raw API key;
    /// for `auth_type=OAuth` this is the current access token (replaced on
    /// refresh). Empty for tombstones.
    pub key: String,
    pub fingerprint: String,
    pub use_count: u64,
    pub last_status: CredentialStatus,
    pub last_status_at_ms: Option<i64>,
    pub last_error_code: Option<u16>,
    pub last_error_reason: Option<String>,
    pub last_error_message: Option<String>,
    pub last_error_reset_at_ms: Option<i64>,
    pub removed_at_ms: Option<i64>,
    /// API key vs OAuth — drives header construction in
    /// `Provider::build_headers` and the OAuth-token-refresh path.
    pub auth_type: AuthType,
    /// OAuth refresh token, rotated on each refresh. None for API keys.
    pub refresh_token: Option<String>,
    /// Wall-clock millis when the access token expires. None means
    /// non-expiring (API keys) or unknown.
    pub expires_at_ms: Option<i64>,
    /// Per-credential base URL override — set by `CredentialSource::Custom` to
    /// point requests at a user-defined OpenAI-compatible endpoint.
    pub base_url: Option<String>,
    /// Per-provider opaque JSON blob (PKCE verifier, OAuth client_id, etc.).
    /// Used by the OAuth-refresh flow.
    pub extra_json: Option<String>,
    /// Latest provider rate-limit headers observed for this credential.
    /// In-memory only — snapshots expire well within the bucket reset window
    /// (seconds-to-minutes), so persisting across restart would just mean
    /// acting on stale data. Populated by `report_rate_limit_headers`.
    pub rate_limit: Option<RateLimitSnapshot>,
}

impl PooledCredential {
    /// True when this credential should never be returned by `pick()`.
    pub fn is_removed(&self) -> bool {
        self.removed_at_ms.is_some()
    }

    /// True when this credential is currently cooling and not yet recovered.
    pub fn is_cooling(&self, now_ms: i64) -> bool {
        !self.is_removed()
            && matches!(self.last_status, CredentialStatus::Exhausted)
            && self
                .last_error_reset_at_ms
                .is_some_and(|until| until > now_ms)
    }

    /// True when this credential is available for `pick()` right now.
    pub fn is_available(&self, now_ms: i64) -> bool {
        !self.is_removed() && !self.is_cooling(now_ms)
    }

    /// True when the latest provider rate-limit headers say this credential
    /// is near-empty *and* far enough from reset that rotating to a sibling
    /// is cheaper than waiting.
    ///
    /// Returns false when:
    /// - no snapshot has been captured (yet);
    /// - `threshold_pct == 0` (operator disabled the feature);
    /// - the bucket's reset window is within `min_window_secs` (waiting is cheaper);
    /// - every reported bucket has more than `threshold_pct` headroom;
    /// - the snapshot is older than its own reset (provider has already replenished).
    ///
    /// This is a hint, not a hard block — `pick_force` ignores it when
    /// nothing else is available so we never *fail* on proactive avoidance.
    pub fn is_proactively_throttled(
        &self,
        now_ms: i64,
        threshold_pct: u8,
        min_window_secs: u64,
    ) -> bool {
        // Defense in depth: tombstoned credentials are already excluded by
        // `is_available`, but a future caller asking the predicate directly
        // must not see a stale-snapshot tombstone reported as "throttled" —
        // that would imply it's a candidate for use.
        if self.is_removed() {
            return false;
        }
        if threshold_pct == 0 {
            return false;
        }
        let Some(snap) = self.rate_limit.as_ref() else {
            return false;
        };
        if snap.is_stale(now_ms) {
            return false;
        }
        let min_window_ms = (min_window_secs as i64).saturating_mul(1000);
        let buckets = [snap.requests, snap.tokens];
        buckets.into_iter().flatten().any(|b| {
            // Stale or already-replenished bucket: don't act on it.
            if b.reset_at_ms <= now_ms {
                return false;
            }
            // Within the "just wait it out" window: rotating costs more than
            // it saves (warmup on the sibling, lost in-flight context).
            if b.reset_at_ms - now_ms <= min_window_ms {
                return false;
            }
            b.remaining_pct() < threshold_pct
        })
    }
}

impl std::fmt::Debug for PooledCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledCredential")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("priority", &self.priority)
            .field("source", &self.source)
            // SECURITY: key is redacted. Never include `self.key` here, even
            // truncated — a partial key in a log can still be a password
            // grease attack vector. Use `fingerprint` for identification.
            .field("key", &format_args!("<redacted len={}>", self.key.len()))
            .field("fingerprint", &self.fingerprint)
            .field("use_count", &self.use_count)
            .field("last_status", &self.last_status)
            .field("last_status_at_ms", &self.last_status_at_ms)
            .field("last_error_code", &self.last_error_code)
            .field("last_error_reason", &self.last_error_reason)
            .field("last_error_message", &self.last_error_message)
            .field("last_error_reset_at_ms", &self.last_error_reset_at_ms)
            .field("removed_at_ms", &self.removed_at_ms)
            .field("auth_type", &self.auth_type)
            // SECURITY: refresh_token is also redacted — same logic as `key`.
            .field(
                "refresh_token",
                &format_args!(
                    "<{}>",
                    self.refresh_token
                        .as_ref()
                        .map(|t| format!("redacted len={}", t.len()))
                        .unwrap_or_else(|| "none".into())
                ),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("base_url", &self.base_url)
            .field("extra_json", &self.extra_json)
            .field("rate_limit", &self.rate_limit)
            .finish()
    }
}

// ── Clock seam (test-injectable) ─────────────────────────────────────────

/// Wall-clock seam. Production is `SystemClock`; tests inject a mock that
/// returns deterministic millis so cooldown logic can be exercised without
/// `tokio::time::sleep`.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> i64;
}

/// Production clock — `SystemTime::now()`.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

// ── Errors ────────────────────────────────────────────────────────────────

/// Outcome of resolving an operator-supplied credential target.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No credential matched the target.
    #[error("No credential matches '{0}'")]
    NotFound(String),
    /// Multiple credentials matched (e.g. by ambiguous label).
    #[error("Target '{0}' is ambiguous — {1} matches")]
    Ambiguous(String, usize),
    /// Index is out of bounds.
    #[error("Index {0} is out of bounds (pool has {1} entries)")]
    OutOfRange(usize, usize),
}

// ── Pool inner state ──────────────────────────────────────────────────────

#[derive(Debug)]
struct PoolInner {
    creds: Vec<PooledCredential>,
    strategy: CredentialStrategy,
    cooldown: Duration,
    current_id: Option<String>,
    active_leases: HashMap<String, u32>,
    max_concurrent_per_credential: u32,
    suppressed: HashSet<(String, String)>, // (source_kind, source_value)
    /// Track which exhaustion warnings we've already emitted so we don't
    /// spam logs on every subsequent failed `pick()`. Cleared on any
    /// recovery (cooldown expiry or operator reset).
    emitted_exhausted_warn: bool,
    /// Proactive rate-limit avoidance threshold (% remaining). `0` disables
    /// the feature — pick() behaves identically to the reactive-only path.
    proactive_threshold_pct: u8,
    /// Minimum seconds-until-reset for proactive avoidance to kick in.
    proactive_min_window_secs: u64,
}

// ── CredentialPool ────────────────────────────────────────────────────────

/// Per-provider pool of API credentials with cooldown, leasing, and persistence.
///
/// Pools are constructed via [`CredentialPool::load`] which reconciles three
/// sources of truth: the DB (persisted state from prior runs), the
/// `LlmConfig.api_keys` Vec, and provider-default environment variables.
/// Removed sources are skipped on re-seed via the `credential_suppressions`
/// table.
///
/// Concurrency: `Arc<Mutex<PoolInner>>` — the lock is held for the duration
/// of `pick`, `report_*`, and lease checkout. All lease bookkeeping happens
/// behind the same lock; `LeaseHandle::drop` re-acquires it to decrement.
pub struct CredentialPool {
    provider: Provider,
    db_path: Option<std::path::PathBuf>,
    inner: Arc<Mutex<PoolInner>>,
    clock: Arc<dyn Clock>,
    /// Per-credential refresh locks. Serializes concurrent OAuth refreshes
    /// against the same credential id so a single-use refresh_token isn't
    /// burned twice. Held only across the HTTP refresh call.
    refresh_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Endpoint override for the OAuth refresh client. `None` uses the
    /// provider-default URLs. Tests inject a `wiremock` endpoint here via
    /// [`Self::set_refresh_endpoint_for_tests`].
    refresh_endpoint_override: Mutex<Option<String>>,
    /// Home directory used by source `write_back` hooks after a successful
    /// OAuth refresh. Mirrors `PoolLoadConfig.oauth_home` (production resolves
    /// via `dirs::home_dir`; tests inject a `tempdir`).
    oauth_home_for_writeback: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for CredentialPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPool")
            .field("provider", &self.provider)
            .field("inner", &self.inner)
            .finish()
    }
}

/// Config inputs for [`CredentialPool::load`]. Decoupled from `LlmConfig` so
/// `LlmFallback` and `AuxLlmConfig` can drive the same loader.
#[derive(Debug, Clone)]
pub struct PoolLoadConfig {
    pub strategy: CredentialStrategy,
    pub cooldown_secs: u64,
    pub max_concurrent_per_credential: u32,
    /// Proactive rate-limit avoidance: skip a credential during `pick()`
    /// when its observed rate-limit headroom drops below this percentage.
    /// `0` disables (the pool stays purely reactive to 429s).
    pub proactive_rate_limit_threshold_pct: u8,
    /// Minimum seconds-until-reset for proactive avoidance to kick in.
    /// Below this, waiting out the bucket is cheaper than rotating.
    pub proactive_rate_limit_min_window_secs: u64,
    /// Resolved (label, key) pairs from `api_keys` config blocks. The loader
    /// computes fingerprints and de-dupes. Labels are operator-friendly tags
    /// for `borg auth list`.
    pub config_entries: Vec<ConfigEntry>,
    /// Provider env vars to scan (typically just `provider.default_env_var()`,
    /// but Anthropic supports multiple). Each present-and-nonempty var is
    /// seeded as `CredentialSource::Env`.
    pub env_vars: Vec<String>,
    /// Override the home directory used by OAuth-credential discoverers.
    /// `None` resolves via `dirs::home_dir()` at load time. Tests inject a
    /// `tempdir` so synthetic credential files don't leak across runs.
    pub oauth_home: Option<std::path::PathBuf>,
}

/// One config-sourced credential, resolved to plaintext.
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub key: String,
    /// Optional operator-friendly label; defaults to `"config[N]"`.
    pub label: Option<String>,
    /// Stable name for the suppression model (e.g. `"llm.api_keys[0]"`).
    pub config_name: String,
}

impl CredentialPool {
    /// Open the pool for `provider`, reconciling DB / config / env into a
    /// coherent set of entries. See module docs for the seeding rules.
    ///
    /// `db_path` is the SQLite file the pool will write to for state
    /// persistence (use the same path the `Database` was opened with — the
    /// pool opens short-lived connections rather than threading the
    /// `rusqlite::Connection` through `Arc<Mutex<…>>`).
    pub async fn load(
        db: &Database,
        db_path: &std::path::Path,
        provider: Provider,
        cfg: PoolLoadConfig,
    ) -> Result<Arc<Self>> {
        Self::load_with_clock(db, db_path, provider, cfg, Arc::new(SystemClock)).await
    }

    /// Like `load`, but with an injected clock for tests.
    pub async fn load_with_clock(
        db: &Database,
        db_path: &std::path::Path,
        provider: Provider,
        cfg: PoolLoadConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>> {
        let conn = db.conn();
        let provider_str = provider.as_str();

        // 1. Load suppression set.
        let mut suppressed: HashSet<(String, String)> = HashSet::new();
        {
            let mut rows = conn
                .query(
                    "SELECT source_kind, source_value FROM credential_suppressions \
                     WHERE provider = ?1",
                    params!(provider_str),
                )
                .await?;
            while let Some(r) = rows.next().await? {
                suppressed.insert((r.get::<String>(0)?, r.get::<String>(1)?));
            }
        }

        // 2. Load persisted rows (state-bearing tombstones included).
        let mut persisted: HashMap<String, PersistedRow> = HashMap::new();
        {
            let mut rows = conn
                .query(
                    "SELECT fingerprint, id, label, priority, source_json, use_count, \
                            last_status, last_status_at_ms, last_error_code, last_error_reason, \
                            last_error_message, last_error_reset_at_ms, removed_at_ms, \
                            auth_type, expires_at_ms, base_url, extra_json \
                     FROM credential_pool WHERE provider = ?1",
                    params!(provider_str),
                )
                .await?;
            while let Some(r) = rows.next().await? {
                let auth_type_str: String = r.get::<String>(13)?;
                let auth_type = match auth_type_str.as_str() {
                    "oauth" => AuthType::OAuth,
                    _ => AuthType::ApiKey,
                };
                let row = PersistedRow {
                    fingerprint: r.get::<String>(0)?,
                    id: r.get::<String>(1)?,
                    label: r.get::<String>(2)?,
                    priority: r.get::<i64>(3)? as u32,
                    source_json: r.get::<String>(4)?,
                    use_count: r.get::<i64>(5)? as u64,
                    last_status: r.get::<String>(6)?,
                    last_status_at_ms: r.get::<Option<i64>>(7)?,
                    last_error_code: r.get::<Option<i64>>(8)?.map(|x| x as u16),
                    last_error_reason: r.get::<Option<String>>(9)?,
                    last_error_message: r.get::<Option<String>>(10)?,
                    last_error_reset_at_ms: r.get::<Option<i64>>(11)?,
                    removed_at_ms: r.get::<Option<i64>>(12)?,
                    auth_type,
                    expires_at_ms: r.get::<Option<i64>>(14)?,
                    base_url: r.get::<Option<String>>(15)?,
                    extra_json: r.get::<Option<String>>(16)?,
                };
                persisted.insert(row.fingerprint.clone(), row);
            }
        }

        // 3. Seed: config entries, then env vars. Suppressed sources skipped.
        let mut live: Vec<PooledCredential> = Vec::new();
        let mut seen_fp: HashSet<String> = HashSet::new();
        let mut next_priority: u32 = 0;

        for entry in &cfg.config_entries {
            if entry.key.trim().is_empty() {
                continue;
            }
            let source = CredentialSource::Config {
                name: entry.config_name.clone(),
            };
            if suppressed.contains(&(source.kind_db().to_string(), source.value_db())) {
                continue;
            }
            let fingerprint = fingerprint(&entry.key);
            if !seen_fp.insert(fingerprint.clone()) {
                continue;
            }
            let label = entry
                .label
                .clone()
                .unwrap_or_else(|| entry.config_name.clone());
            let cred = reconcile_or_new(
                &persisted,
                fingerprint,
                entry.key.clone(),
                label,
                source,
                next_priority,
                clock.now_ms(),
            );
            next_priority += 1;
            live.push(cred);
        }

        for var in &cfg.env_vars {
            let Ok(val) = std::env::var(var) else {
                continue;
            };
            let val = val.trim().to_string();
            if val.is_empty() {
                continue;
            }
            let source = CredentialSource::Env { var: var.clone() };
            if suppressed.contains(&(source.kind_db().to_string(), source.value_db())) {
                continue;
            }
            let fingerprint = fingerprint(&val);
            if !seen_fp.insert(fingerprint.clone()) {
                continue;
            }
            let cred = reconcile_or_new(
                &persisted,
                fingerprint,
                val,
                var.clone(),
                source,
                next_priority,
                clock.now_ms(),
            );
            next_priority += 1;
            live.push(cred);
        }

        // 3b. Seed: OAuth and custom-endpoint credentials discovered on disk.
        //     Discoverers are matched per-provider; sources that find nothing
        //     return Ok(vec![]). Parser errors log + continue — a malformed
        //     credentials file must never crash pool load.
        let oauth_home = cfg.oauth_home.clone().or_else(dirs::home_dir);
        if let Some(home) = oauth_home.as_deref() {
            for discoverer in crate::llm::credential_sources::discoverers_for(provider) {
                let discovered = match discoverer.discover(home) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            target: "cortex::llm::credential_pool",
                            source = discoverer.name(),
                            error = %e,
                            "credential pool: discoverer failed — skipping"
                        );
                        continue;
                    }
                };
                for disc in discovered {
                    // Reject unsafe base_urls before the credential lands in
                    // the pool — see `is_safe_base_url` for the threat model.
                    // Auto-seed quietly skips (logs warn); manual `borg auth
                    // pair` paths surface the error to the operator.
                    if let Some(url) = disc.base_url.as_deref() {
                        if !is_safe_base_url(url) {
                            warn!(
                                target: "cortex::llm::credential_pool",
                                source = discoverer.name(),
                                base_url = url,
                                "credential pool: rejecting unsafe base_url from discoverer — skipping"
                            );
                            continue;
                        }
                    }
                    let source = match discoverer.provider() {
                        Provider::Custom => CredentialSource::Custom {
                            name: discoverer.name().to_string(),
                            base_url: disc.base_url.clone().unwrap_or_default(),
                        },
                        _ => CredentialSource::OAuth {
                            name: discoverer.name().to_string(),
                        },
                    };
                    if suppressed.contains(&(source.kind_db().to_string(), source.value_db())) {
                        continue;
                    }
                    let fp = fingerprint(&disc.access_token);
                    if !seen_fp.insert(fp.clone()) {
                        continue;
                    }
                    // Qwen-CLI ships an API key with a base_url override, not
                    // an OAuth refresh pair. Everything else is OAuth.
                    let auth_type = if disc.refresh_token.is_some() {
                        AuthType::OAuth
                    } else {
                        AuthType::ApiKey
                    };
                    let cred = reconcile_or_new_oauth(
                        &persisted,
                        fp,
                        disc,
                        source,
                        auth_type,
                        next_priority,
                        clock.now_ms(),
                    );
                    next_priority += 1;
                    live.push(cred);
                }
            }
        }

        // 4. Carry forward removed-tombstone rows for `borg auth list` history.
        for (fp, row) in &persisted {
            if !seen_fp.contains(fp) && row.removed_at_ms.is_some() {
                if let Ok(source) = serde_json::from_str::<CredentialSource>(&row.source_json) {
                    live.push(PooledCredential {
                        id: row.id.clone(),
                        label: row.label.clone(),
                        priority: row.priority,
                        source,
                        key: String::new(), // no live key — tombstone
                        fingerprint: fp.clone(),
                        use_count: row.use_count,
                        last_status: CredentialStatus::from_db(&row.last_status),
                        last_status_at_ms: row.last_status_at_ms,
                        last_error_code: row.last_error_code,
                        last_error_reason: row.last_error_reason.clone(),
                        last_error_message: row.last_error_message.clone(),
                        last_error_reset_at_ms: row.last_error_reset_at_ms,
                        removed_at_ms: row.removed_at_ms,
                        auth_type: row.auth_type,
                        refresh_token: None, // tombstone — never resurrect refresh secret
                        expires_at_ms: row.expires_at_ms,
                        base_url: row.base_url.clone(),
                        extra_json: row.extra_json.clone(),
                        rate_limit: None,
                    });
                }
            }
        }

        // 5. Prune persisted rows whose source disappeared from env/config and
        //    that aren't tombstoned (so `borg auth list` still shows manual
        //    removals); the live ones we already reconciled above.
        let live_fps: HashSet<String> = live.iter().map(|c| c.fingerprint.clone()).collect();
        let to_prune: Vec<String> = persisted
            .keys()
            .filter(|fp| !live_fps.contains(*fp))
            .cloned()
            .collect();
        if !to_prune.is_empty() {
            for fp in to_prune {
                conn.execute(
                    "DELETE FROM credential_pool WHERE provider = ?1 AND fingerprint = ?2",
                    params!(provider_str, fp),
                )
                .await?;
            }
        }

        // 6. Normalize priorities (live first, tombstones at the end).
        live.sort_by(|a, b| {
            a.removed_at_ms
                .is_some()
                .cmp(&b.removed_at_ms.is_some())
                .then(a.priority.cmp(&b.priority))
        });
        for (i, c) in live.iter_mut().enumerate() {
            c.priority = i as u32;
        }

        // 7. Persist (write-through reflects normalized priorities).
        for c in &live {
            upsert_row(conn, provider_str, c).await?;
        }

        let inner = PoolInner {
            creds: live,
            strategy: cfg.strategy,
            cooldown: Duration::from_secs(cfg.cooldown_secs.max(1)),
            current_id: None,
            active_leases: HashMap::new(),
            max_concurrent_per_credential: cfg.max_concurrent_per_credential.max(1),
            suppressed,
            emitted_exhausted_warn: false,
            proactive_threshold_pct: cfg.proactive_rate_limit_threshold_pct,
            proactive_min_window_secs: cfg.proactive_rate_limit_min_window_secs,
        };

        Ok(Arc::new(Self {
            provider,
            db_path: Some(db_path.to_path_buf()),
            inner: Arc::new(Mutex::new(inner)),
            clock,
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            refresh_endpoint_override: Mutex::new(None),
            oauth_home_for_writeback: cfg.oauth_home,
        }))
    }

    /// Test-only: redirect OAuth refresh calls to the given URL (e.g. a
    /// `wiremock` server). Production code never sets this.
    ///
    /// The override applies only to whichever refresh branch matches this
    /// pool's `provider`. A single pool can't have two providers, so the
    /// override is unambiguously scoped — but the body/payload shape sent
    /// to the wiremock URL still matches the pool's provider.
    #[doc(hidden)]
    pub fn set_refresh_endpoint_for_tests(&self, url: Option<String>) {
        let mut g = self
            .refresh_endpoint_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = url;
    }

    /// Provider this pool serves.
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// True when the pool has any non-removed credential (regardless of cooling).
    pub fn has_credentials(&self) -> bool {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds.iter().any(|c| !c.is_removed())
    }

    /// True when at least one credential is pickable right now (not removed, not cooling).
    pub fn has_available(&self) -> bool {
        let now = self.clock.now_ms();
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds.iter().any(|c| c.is_available(now))
    }

    /// String name of this pool's provider (e.g. `"openrouter"`). Used by
    /// the daemon's admin RPC layer to render results without exposing the
    /// underlying `Provider` enum to the proto schema.
    pub fn provider_str(&self) -> &'static str {
        self.provider.as_str()
    }

    /// The strategy + cooldown + concurrency knobs the pool was constructed
    /// with. Reads are O(1) under the lock.
    pub fn cooldown_secs(&self) -> u64 {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.cooldown.as_secs()
    }

    /// Soft per-credential concurrency cap.
    pub fn max_concurrent_per_credential(&self) -> u32 {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.max_concurrent_per_credential
    }

    /// Number of credentials available right now.
    pub fn len_active(&self) -> usize {
        let now = self.clock.now_ms();
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds.iter().filter(|c| c.is_available(now)).count()
    }

    /// True when every credential is either removed or cooling. Inverse of `has_available`.
    pub fn is_exhausted(&self) -> bool {
        !self.has_available()
    }

    /// Earliest wall-clock millis at which any cooling credential recovers, or
    /// `None` if every non-removed credential is either available or permanently
    /// removed.
    pub fn next_recovery_at_ms(&self) -> Option<i64> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds
            .iter()
            .filter(|c| !c.is_removed())
            .filter_map(|c| c.last_error_reset_at_ms)
            .min()
    }

    /// True when at least one cooling (non-removed) credential will recover
    /// within `within`. Used by the LLM caller to *defer* provider fallback on
    /// a transient 429/402 when a sibling key is about to come back — jumping
    /// to the fallback provider while the primary pool is still draining its
    /// cooldown burns budget on a degraded provider for no reason.
    ///
    /// Returns false when all entries are permanently failed (tombstoned with
    /// no reset time), when the pool is empty, or when every cooldown is
    /// outside the recovery window (in which case fallback should fire).
    pub fn may_recover_within(&self, within: Duration) -> bool {
        let now = self.clock.now_ms();
        let horizon = now + within.as_millis() as i64;
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live_credential_count = g.creds.iter().filter(|c| !c.is_removed()).count();
        if live_credential_count < 2 {
            return false;
        }
        g.creds.iter().any(|c| {
            !c.is_removed()
                && c.last_error_reset_at_ms
                    .map(|r| r > now && r <= horizon)
                    .unwrap_or(false)
        })
    }

    /// Snapshot of all entries (including removed tombstones) for `borg auth list`.
    pub fn entries(&self) -> Vec<PooledCredential> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds.clone()
    }

    /// Best-effort persist of one credential row. Logs at `warn` on failure so
    /// operators see read-only-mount / locked-db / corrupted-DB errors instead
    /// of silently losing pool state across restart.
    ///
    /// libsql's connection API is async — we fire-and-forget the write onto the
    /// current tokio runtime so the sync caller (e.g. `report_success`) keeps
    /// its non-blocking contract. If no runtime is current (rare; only the
    /// startup `borg auth` CLI today), the persist is logged and skipped.
    fn persist_credential(&self, cred: &PooledCredential) {
        let Some(path) = self.db_path.as_ref() else {
            return;
        };
        spawn_persist_task(
            path.clone(),
            self.provider.as_str().to_string(),
            PersistOp::Upsert(cred.clone()),
        );
    }

    /// Best-effort persist of a suppression row. Same logging contract as
    /// `persist_credential`.
    fn persist_suppression_row(&self, source: &CredentialSource, now: i64) {
        let Some(path) = self.db_path.as_ref() else {
            return;
        };
        spawn_persist_task(
            path.clone(),
            self.provider.as_str().to_string(),
            PersistOp::Suppress {
                source: source.clone(),
                now_ms: now,
            },
        );
    }

    /// Best-effort delete of a suppression row. Used when an operator re-adds
    /// a previously-removed credential — the tombstone is resurrected and the
    /// matching suppression must be cleared so re-seeding from env/config
    /// doesn't immediately re-tombstone it on next start.
    fn delete_suppression_row(&self, source_kind: &str, source_value: &str) {
        let Some(path) = self.db_path.as_ref() else {
            return;
        };
        spawn_persist_task(
            path.clone(),
            self.provider.as_str().to_string(),
            PersistOp::Unsuppress {
                source_kind: source_kind.to_string(),
                source_value: source_value.to_string(),
            },
        );
    }

    /// Current strategy.
    pub fn strategy(&self) -> CredentialStrategy {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .strategy
    }

    /// Currently-selected credential (set by the last `pick()` / `acquire_lease()`).
    pub fn current(&self) -> Option<PooledCredential> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = g.current_id.as_ref()?;
        g.creds.iter().find(|c| &c.id == id).cloned()
    }

    /// Non-mutating: returns `current()` if still available, else the first
    /// available credential by current strategy. Does **not** bump `use_count`
    /// or update `current_id`.
    pub fn peek(&self) -> Option<PooledCredential> {
        let now = self.clock.now_ms();
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(id) = &g.current_id {
            if let Some(c) = g.creds.iter().find(|c| &c.id == id) {
                if c.is_available(now) {
                    return Some(c.clone());
                }
            }
        }
        g.creds
            .iter()
            .filter(|c| c.is_available(now))
            .min_by_key(|c| c.priority)
            .cloned()
    }

    /// Strategy-driven selection. Updates `current_id` and bumps `use_count`.
    /// Returns `None` when every credential is removed or cooling — callers
    /// should fall through to `pick_force` or surface an exhausted error.
    pub fn pick(&self) -> Option<PooledCredential> {
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let threshold_pct = g.proactive_threshold_pct;
        let min_window_secs = g.proactive_min_window_secs;
        // Two-pass filter: first exclude reactively-cooling AND proactively-
        // throttled credentials. If every credential is proactively throttled
        // but otherwise available, fall back to the original filter — the
        // pool should still pick *something*, since rotating to nothing
        // strictly worsens latency vs. the would-be 429.
        let avail_strict: Vec<usize> = g
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.is_available(now)
                    && !c.is_proactively_throttled(now, threshold_pct, min_window_secs)
            })
            .map(|(i, _)| i)
            .collect();
        let avail: Vec<usize> = if avail_strict.is_empty() {
            g.creds
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_available(now))
                .map(|(i, _)| i)
                .collect()
        } else {
            avail_strict
        };

        if avail.is_empty() {
            if !g.emitted_exhausted_warn {
                let next = g
                    .creds
                    .iter()
                    .filter(|c| !c.is_removed())
                    .filter_map(|c| c.last_error_reset_at_ms)
                    .min();
                let total = g.creds.iter().filter(|c| !c.is_removed()).count();
                warn!(
                    target: "cortex::llm::credential_pool",
                    provider = %self.provider,
                    pool_size = total,
                    next_recovery_at_ms = ?next,
                    "credential pool exhausted"
                );
                g.emitted_exhausted_warn = true;
            }
            return None;
        }

        let strategy = g.strategy;
        // `avail` is non-empty (checked above), so each strategy's selection
        // is infallible. Use `?` so a future change to the empty-check above
        // can't accidentally trigger a panic in production.
        let picked_idx = match strategy {
            CredentialStrategy::FillFirst => {
                // Lowest-priority available.
                *avail.iter().min_by_key(|&&i| g.creds[i].priority)?
            }
            CredentialStrategy::RoundRobin => {
                // First available after the current one (by priority);
                // after picking, rotate the picked entry to the back.
                let after = g
                    .current_id
                    .as_ref()
                    .and_then(|id| g.creds.iter().position(|c| &c.id == id));
                match after {
                    Some(cur_idx) => avail
                        .iter()
                        .find(|&&i| g.creds[i].priority > g.creds[cur_idx].priority)
                        .or_else(|| avail.iter().min_by_key(|&&i| g.creds[i].priority))
                        .copied()?,
                    None => *avail.iter().min_by_key(|&&i| g.creds[i].priority)?,
                }
            }
            CredentialStrategy::LeastUsed => *avail.iter().min_by(|&&a, &&b| {
                g.creds[a]
                    .use_count
                    .cmp(&g.creds[b].use_count)
                    .then(g.creds[a].priority.cmp(&g.creds[b].priority))
            })?,
            CredentialStrategy::Random => {
                let mut rng = rand::rng();
                *avail.choose(&mut rng)?
            }
        };

        g.creds[picked_idx].use_count += 1;
        let picked = g.creds[picked_idx].clone();
        g.current_id = Some(picked.id.clone());
        g.emitted_exhausted_warn = false;
        debug!(
            target: "cortex::llm::credential_pool",
            id = %picked.id,
            label = %picked.label,
            strategy = %strategy,
            "pick"
        );
        // Drop the guard before disk I/O — SQLite open+write can block other
        // pool callers (lease/report/pick) and `acquire_lease` runs on every
        // request in the failover loop.
        drop(g);
        self.persist_credential(&picked);
        Some(picked)
    }

    /// Salvage attempt when every credential is cooling. Returns the credential
    /// whose cooldown expires soonest (regardless of strategy). Used by the
    /// failover loop as a last-gasp before propagating an exhaustion error.
    /// Returns `None` only when every entry is removed.
    pub fn pick_force(&self) -> Option<PooledCredential> {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pick_id = g
            .creds
            .iter()
            .filter(|c| !c.is_removed())
            .min_by_key(|c| {
                // Cooling entries: soonest reset_at wins.
                // Available entries: treat as i64::MIN so they're preferred.
                c.last_error_reset_at_ms.unwrap_or(i64::MIN)
            })
            .map(|c| c.id.clone())?;
        // Bump use_count so the salvage attempt shows up in least_used
        // accounting and in `borg auth list` — otherwise the pool reports
        // "used 0×" against a credential the loop just hammered.
        let entry = g.creds.iter_mut().find(|c| c.id == pick_id)?;
        entry.use_count = entry.use_count.saturating_add(1);
        let cred = entry.clone();
        g.current_id = Some(cred.id.clone());
        debug!(
            target: "cortex::llm::credential_pool",
            id = %cred.id,
            label = %cred.label,
            "pick_force"
        );
        drop(g);
        self.persist_credential(&cred);
        Some(cred)
    }

    /// Record observed rate-limit headers for `credential_id`. Overwrites
    /// any prior snapshot — provider headers are the freshest source of
    /// truth about the bucket. Held in memory only; not persisted.
    ///
    /// Called from both the success and failure paths so the pool always
    /// acts on the most recent observation, including the snapshot that
    /// accompanied the 429 that just cooled the key.
    pub fn report_rate_limit_headers(&self, credential_id: &str, snapshot: RateLimitSnapshot) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(c) = g.creds.iter_mut().find(|c| c.id == credential_id) else {
            return;
        };
        if c.is_removed() {
            return;
        }
        debug!(
            target: "cortex::llm::credential_pool",
            id = %c.id,
            requests = ?snapshot.requests,
            tokens = ?snapshot.tokens,
            "rate-limit snapshot captured"
        );
        c.rate_limit = Some(snapshot);
    }

    /// Record a successful request against `credential_id`. Bumps `use_count`,
    /// clears any prior error state, and resets any cooldown (a success means
    /// the throttle lifted).
    pub fn report_success(&self, credential_id: &str) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(c) = g.creds.iter_mut().find(|c| c.id == credential_id) else {
            return;
        };
        if c.is_removed() {
            return;
        }
        // `use_count` was already bumped at pick time; report_success is the
        // "outcome arrived" hook. Don't double-bump.
        c.last_status = CredentialStatus::Ok;
        c.last_status_at_ms = Some(self.clock.now_ms());
        c.last_error_code = None;
        c.last_error_reason = None;
        c.last_error_message = None;
        c.last_error_reset_at_ms = None;
        g.emitted_exhausted_warn = false;
        let cred = g.creds.iter().find(|c| c.id == credential_id).cloned();
        drop(g);
        if let Some(c) = cred {
            self.persist_credential(&c);
        }
    }

    /// Record a failed request. Maps `reason` to the appropriate state
    /// transition: Auth/AuthPermanent → soft-delete; RateLimit/Billing →
    /// cooldown (via `retry_after` or configured TTL); everything else is
    /// a no-op on the pool (transport / payload concerns).
    pub fn report_failure(
        &self,
        credential_id: &str,
        reason: FailoverReason,
        status_code: Option<u16>,
        retry_after: Option<Duration>,
        error_message: Option<&str>,
    ) {
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cooldown = g.cooldown;
        let Some(c) = g.creds.iter_mut().find(|c| c.id == credential_id) else {
            return;
        };
        if c.is_removed() {
            return;
        }
        let truncated = error_message.map(truncate_error_msg);

        match reason {
            FailoverReason::Auth | FailoverReason::AuthPermanent => {
                c.removed_at_ms = Some(now);
                c.last_status = CredentialStatus::Exhausted;
                c.last_status_at_ms = Some(now);
                c.last_error_code = status_code;
                c.last_error_reason = Some(reason.to_string());
                c.last_error_message = truncated;
                c.last_error_reset_at_ms = None;
                info!(
                    target: "cortex::llm::credential_pool",
                    id = %c.id,
                    label = %c.label,
                    fingerprint = %c.fingerprint,
                    reason = %reason,
                    status_code = ?status_code,
                    "credential removed (auth failure)"
                );
            }
            FailoverReason::RateLimit | FailoverReason::Billing => {
                let dur = retry_after
                    .map(|d| d.as_secs().clamp(1, COOLDOWN_MAX_SECS))
                    .unwrap_or_else(|| cooldown.as_secs().clamp(1, COOLDOWN_MAX_SECS));
                let new_until = now + (dur as i64) * 1000;
                // Extend, never shorten — a second 429 with a longer hint
                // should win.
                c.last_error_reset_at_ms = Some(match c.last_error_reset_at_ms {
                    Some(existing) if existing > new_until => existing,
                    _ => new_until,
                });
                c.last_status = CredentialStatus::Exhausted;
                c.last_status_at_ms = Some(now);
                c.last_error_code = status_code;
                c.last_error_reason = Some(reason.to_string());
                c.last_error_message = truncated;
                info!(
                    target: "cortex::llm::credential_pool",
                    id = %c.id,
                    reason = %reason,
                    status_code = ?status_code,
                    cooldown_secs = dur,
                    "credential cooled"
                );
            }
            _ => {
                // Transport / payload / format / overflow — not a credential concern.
            }
        }

        let cred = g.creds.iter().find(|c| c.id == credential_id).cloned();
        drop(g);
        if let Some(c) = cred {
            self.persist_credential(&c);
        }
    }

    /// Clear all `Exhausted` entries back to `Ok`. Does **not** touch removed
    /// entries. Returns the count cleared. Persists.
    pub fn reset_cooldowns(&self) -> usize {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cleared = 0;
        for c in g.creds.iter_mut() {
            if c.is_removed() {
                continue;
            }
            if matches!(c.last_status, CredentialStatus::Exhausted) {
                c.last_status = CredentialStatus::Ok;
                c.last_error_reset_at_ms = None;
                cleared += 1;
            }
        }
        g.emitted_exhausted_warn = false;
        let snapshot = g.creds.clone();
        drop(g);
        for c in &snapshot {
            self.persist_credential(c);
        }
        cleared
    }

    /// Runtime strategy change. Persists nothing on its own — the LLM config
    /// owns persistence of the strategy. Use this when an operator changes
    /// `llm.credential_strategy` and we need the live pool to honor it.
    pub fn set_strategy(&self, strategy: CredentialStrategy) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.strategy = strategy;
    }

    /// Add a credential at runtime. If the fingerprint already matches an
    /// existing entry, returns that entry unchanged (idempotent).
    ///
    /// **Persistence note.** We intentionally never write plaintext keys to
    /// the DB; a runtime-added `Manual` credential therefore *does not survive
    /// restart*. The operator must also add the key to config or set the
    /// matching env var if they want it to come back. (`CredentialSource::Env`
    /// / `Config` survive restart via re-seeding.) Tombstones for any source
    /// always survive — `remove_index` writes a suppression row.
    ///
    /// For OAuth / custom-endpoint credentials, prefer [`Self::add_entry_full`]
    /// which threads `auth_type`, `refresh_token`, `expires_at_ms`, and
    /// `base_url` through. This method hard-codes `AuthType::ApiKey`.
    pub fn add_entry(
        &self,
        key: String,
        label: String,
        source: CredentialSource,
    ) -> PooledCredential {
        self.add_entry_full(key, label, source, AuthType::ApiKey, None, None, None, None)
    }

    /// Full-fidelity add: preserves OAuth/custom-endpoint metadata.
    ///
    /// Used by `borg auth pair` for OAuth credentials, and by future
    /// `borg auth add --base-url` for custom endpoints. Idempotent on
    /// fingerprint match — a live entry is returned unchanged; a tombstoned
    /// entry is resurrected with the new metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn add_entry_full(
        &self,
        key: String,
        label: String,
        source: CredentialSource,
        auth_type: AuthType,
        refresh_token: Option<String>,
        expires_at_ms: Option<i64>,
        base_url: Option<String>,
        extra_json: Option<String>,
    ) -> PooledCredential {
        let fingerprint = fingerprint(&key);
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Fingerprint collision against a live entry → idempotent no-op.
        // Against a tombstone → resurrect (clear removal + suppression so the
        // pool actually starts using it again). Without this branch, a
        // remove-then-readd of the same plaintext silently returned the dead
        // entry, giving the operator a "success" message and a pool that
        // never used the key.
        if let Some(pos) = g.creds.iter().position(|c| c.fingerprint == fingerprint) {
            if g.creds[pos].is_removed() {
                let suppression_key = (
                    g.creds[pos].source.kind_db().to_string(),
                    g.creds[pos].source.value_db(),
                );
                let entry = &mut g.creds[pos];
                entry.removed_at_ms = None;
                entry.key = key;
                entry.source = source;
                entry.auth_type = auth_type;
                entry.refresh_token = refresh_token;
                entry.expires_at_ms = expires_at_ms;
                entry.base_url = base_url;
                entry.extra_json = extra_json;
                entry.last_status = CredentialStatus::Ok;
                entry.last_status_at_ms = Some(now);
                entry.last_error_code = None;
                entry.last_error_reason = None;
                entry.last_error_message = None;
                entry.last_error_reset_at_ms = None;
                // The previous rate-limit snapshot is from before the
                // credential was tombstoned — almost certainly stale, and
                // we want a fresh observation before acting on it.
                entry.rate_limit = None;
                let resurrected = entry.clone();
                let had_suppression = g.suppressed.remove(&suppression_key);
                g.emitted_exhausted_warn = false;
                drop(g);
                self.persist_credential(&resurrected);
                if had_suppression {
                    self.delete_suppression_row(&suppression_key.0, &suppression_key.1);
                }
                return resurrected;
            }
            return g.creds[pos].clone();
        }
        // Assign a priority strictly above every existing entry (live or
        // tombstoned) so priorities stay unique — tombstones keep their
        // historical slot until pruned.
        let priority = g
            .creds
            .iter()
            .map(|c| c.priority)
            .max()
            .map(|p| p + 1)
            .unwrap_or(0);
        let cred = PooledCredential {
            id: random_id(),
            label,
            priority,
            source,
            key,
            fingerprint,
            use_count: 0,
            last_status: CredentialStatus::Ok,
            last_status_at_ms: Some(now),
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at_ms: None,
            removed_at_ms: None,
            auth_type,
            refresh_token,
            expires_at_ms,
            base_url,
            extra_json,
            rate_limit: None,
        };
        g.creds.push(cred.clone());
        g.emitted_exhausted_warn = false;
        drop(g);
        self.persist_credential(&cred);
        cred
    }

    /// Soft-delete by 1-based index. Also writes a suppression row so the
    /// same source isn't re-seeded on next start. Returns the removed entry,
    /// or `None` for an out-of-range index.
    pub fn remove_index(&self, one_based: usize) -> Option<PooledCredential> {
        if one_based == 0 {
            return None;
        }
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Convert 1-based to a position in `creds` ordered by current Vec layout.
        let idx = one_based - 1;
        if idx >= g.creds.len() {
            return None;
        }
        let cred = &mut g.creds[idx];
        if cred.is_removed() {
            return Some(cred.clone());
        }
        cred.removed_at_ms = Some(now);
        cred.key.clear();
        let removed = cred.clone();
        let source = removed.source.clone();
        g.suppressed
            .insert((source.kind_db().to_string(), source.value_db()));
        drop(g);

        self.persist_credential(&removed);
        self.persist_suppression_row(&source, now);
        Some(removed)
    }

    /// Resolve an operator-supplied target string to a (0-based-index, credential)
    /// pair. Matching priority: id prefix → exact label → numeric 1-based index.
    pub fn resolve_target(
        &self,
        target: &str,
    ) -> std::result::Result<(usize, PooledCredential), ResolveError> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Try id (full or unique 4+ hex prefix).
        let id_matches: Vec<usize> = g
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.id == target || (target.len() >= 4 && c.id.starts_with(target)))
            .map(|(i, _)| i)
            .collect();
        if id_matches.len() == 1 {
            let i = id_matches[0];
            return Ok((i, g.creds[i].clone()));
        }
        if id_matches.len() > 1 {
            return Err(ResolveError::Ambiguous(target.into(), id_matches.len()));
        }

        // Try exact label.
        let label_matches: Vec<usize> = g
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.label == target)
            .map(|(i, _)| i)
            .collect();
        if label_matches.len() == 1 {
            let i = label_matches[0];
            return Ok((i, g.creds[i].clone()));
        }
        if label_matches.len() > 1 {
            return Err(ResolveError::Ambiguous(target.into(), label_matches.len()));
        }

        // Try 1-based numeric index.
        if let Ok(n) = target.parse::<usize>() {
            if n == 0 || n > g.creds.len() {
                return Err(ResolveError::OutOfRange(n, g.creds.len()));
            }
            let i = n - 1;
            return Ok((i, g.creds[i].clone()));
        }

        Err(ResolveError::NotFound(target.into()))
    }

    /// Acquire a lease on a credential, preferring below-cap entries by
    /// `(active_leases, priority)` ascending. When every available credential
    /// is at the cap, returns the least-leased anyway. Returns `None` only
    /// when no non-removed credential exists or every credential is cooling
    /// (after `pick_force` would still have returned a tombstone-only set).
    pub fn acquire_lease(&self, requested_id: Option<&str>) -> Option<LeaseHandle> {
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Explicit-id path: requested key must exist and not be removed. The
        // soft concurrency cap is *advisory* here (explicit
        // acquire bypasses the cap). Cooldown is also bypassed: the caller
        // asked for this specific credential and the failover loop's
        // `pick_force` path relies on this to attempt a salvage request
        // against a still-cooling key.
        if let Some(want) = requested_id {
            let idx = g
                .creds
                .iter()
                .position(|c| c.id == want && !c.is_removed())?;
            g.creds[idx].use_count += 1;
            let snapshot = g.creds[idx].clone();
            let id = snapshot.id.clone();
            let count = g.active_leases.entry(id.clone()).or_insert(0);
            *count += 1;
            g.current_id = Some(id.clone());
            drop(g);
            self.persist_credential(&snapshot);
            return Some(LeaseHandle {
                pool: self.inner.clone(),
                credential_id: id,
            });
        }

        let cap = g.max_concurrent_per_credential;
        let threshold_pct = g.proactive_threshold_pct;
        let min_window_secs = g.proactive_min_window_secs;
        let available_all: Vec<usize> = g
            .creds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_available(now))
            .map(|(i, _)| i)
            .collect();
        if available_all.is_empty() {
            return None;
        }
        // Two-pass: prefer credentials that aren't proactively throttled.
        // If every available credential is throttled (every bucket near
        // empty), fall through to the full set — picking *something* still
        // beats failing entirely.
        let available_strict: Vec<usize> = available_all
            .iter()
            .copied()
            .filter(|&i| !g.creds[i].is_proactively_throttled(now, threshold_pct, min_window_secs))
            .collect();
        let available: Vec<usize> = if available_strict.is_empty() {
            available_all
        } else {
            available_strict
        };
        // Prefer below-cap by (leases, priority).
        let below_cap: Vec<usize> = available
            .iter()
            .copied()
            .filter(|&i| g.active_leases.get(&g.creds[i].id).copied().unwrap_or(0) < cap)
            .collect();
        // Pick from `below_cap` if any are under the cap; otherwise from
        // `available` (every available credential is at the cap — return
        // the least-leased anyway). `available` is non-empty (checked above).
        let candidates: &[usize] = if below_cap.is_empty() {
            &available
        } else {
            &below_cap
        };
        let pool_idx = candidates
            .iter()
            .min_by(|&&a, &&b| {
                let la = g.active_leases.get(&g.creds[a].id).copied().unwrap_or(0);
                let lb = g.active_leases.get(&g.creds[b].id).copied().unwrap_or(0);
                la.cmp(&lb)
                    .then(g.creds[a].priority.cmp(&g.creds[b].priority))
            })
            .copied()?;

        g.creds[pool_idx].use_count += 1;
        let snapshot = g.creds[pool_idx].clone();
        let id = snapshot.id.clone();
        let count = g.active_leases.entry(id.clone()).or_insert(0);
        *count += 1;
        g.current_id = Some(id.clone());
        drop(g);
        self.persist_credential(&snapshot);
        Some(LeaseHandle {
            pool: self.inner.clone(),
            credential_id: id,
        })
    }

    /// Like [`acquire_lease`], but refreshes the OAuth access token first if
    /// it's expired (or within the 60-second skew window). Non-OAuth
    /// credentials and OAuth credentials without a refresh_token take the
    /// fast path — identical to `acquire_lease`.
    ///
    /// Concurrency: per-credential `tokio::Mutex` serializes parallel refresh
    /// attempts against the same id so a single-use refresh_token can't be
    /// double-spent. After the lock is acquired, expiration is re-checked —
    /// another caller may have already refreshed.
    ///
    /// Failures: on refresh error, the credential is cooled (5min) but not
    /// removed — the refresh server may simply be flaky. A 401/403 from the
    /// refresh endpoint surfaces as a generic refresh failure (the upstream
    /// 401-handling path on the actual LLM call will tombstone the entry).
    pub async fn acquire_lease_refreshing(
        &self,
        requested_id: Option<&str>,
    ) -> Option<LeaseHandle> {
        let lease = self.acquire_lease(requested_id)?;
        let snapshot = lease.credential()?;
        if !matches!(snapshot.auth_type, AuthType::OAuth) {
            return Some(lease);
        }
        if snapshot.refresh_token.is_none() {
            return Some(lease);
        }
        if !needs_refresh(snapshot.expires_at_ms, self.clock.now_ms()) {
            return Some(lease);
        }

        let entry_lock = {
            let mut locks = self
                .refresh_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks
                .entry(snapshot.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = entry_lock.lock().await;

        // Re-snapshot under the entry lock — a sibling caller may have just
        // refreshed (rotating both access_token AND refresh_token). Using the
        // pre-lock refresh_token would burn the now-invalidated value and
        // produce an `invalid_grant` from the upstream.
        let snapshot = lease.credential()?;
        if !needs_refresh(snapshot.expires_at_ms, self.clock.now_ms()) {
            return Some(lease);
        }
        let Some(refresh_token) = snapshot.refresh_token.clone() else {
            // Sibling-refresh path: another caller wiped refresh_token while
            // we waited. Skip — there's nothing to refresh with.
            return Some(lease);
        };

        match self.refresh_oauth(&refresh_token).await {
            Ok(tokens) => {
                self.apply_refreshed(&snapshot.id, tokens);
                Some(lease)
            }
            Err(e) => {
                warn!(
                    target: "cortex::llm::credential_pool",
                    provider = self.provider.as_str(),
                    credential = %snapshot.id,
                    error = %e,
                    "oauth refresh failed — cooling credential 5min"
                );
                // Reuse the RateLimit arm to cool the credential — semantically
                // equivalent (transient, retryable, scheduled recovery) and it's
                // the only arm that sets `last_error_reset_at_ms`. The error
                // message disambiguates from a true 429 in audit logs.
                self.report_failure(
                    &snapshot.id,
                    FailoverReason::RateLimit,
                    None,
                    Some(Duration::from_secs(300)),
                    Some("oauth_refresh_failed"),
                );
                drop(lease);
                self.acquire_lease(None)
            }
        }
    }

    async fn refresh_oauth(
        &self,
        refresh_token: &str,
    ) -> anyhow::Result<crate::llm::oauth_refresh::RefreshedTokens> {
        use crate::llm::oauth_refresh::{
            refresh_anthropic, refresh_anthropic_against, refresh_codex, refresh_codex_against,
        };
        let override_url = {
            let g = self
                .refresh_endpoint_override
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        match (self.provider, override_url) {
            (Provider::ClaudeCodeOAuth, Some(url)) => {
                refresh_anthropic_against(refresh_token, &url).await
            }
            (Provider::ClaudeCodeOAuth, None) => refresh_anthropic(refresh_token).await,
            (Provider::CodexOAuth, Some(url)) => refresh_codex_against(refresh_token, &url).await,
            (Provider::CodexOAuth, None) => refresh_codex(refresh_token).await,
            (other, _) => Err(anyhow::anyhow!(
                "oauth refresh not implemented for provider {}",
                other.as_str()
            )),
        }
    }

    fn apply_refreshed(&self, id: &str, tokens: crate::llm::oauth_refresh::RefreshedTokens) {
        let snapshot = {
            let mut g = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(c) = g.creds.iter_mut().find(|c| c.id == id) else {
                return;
            };
            c.key = tokens.access_token.clone();
            c.refresh_token = Some(tokens.refresh_token.clone());
            c.expires_at_ms = Some(tokens.expires_at_ms);
            c.clone()
        };
        self.persist_credential(&snapshot);
        // Write the rotated refresh_token back to the source file so the next
        // daemon reload doesn't replay a burnt refresh_token. Best-effort: a
        // write failure logs `warn!` but doesn't fail the in-memory refresh
        // (the new tokens are already live in the pool).
        if let CredentialSource::OAuth { name } = &snapshot.source {
            if let Some(discoverer) = crate::llm::credential_sources::discoverer_by_name(name) {
                let home = self
                    .oauth_home_for_writeback
                    .clone()
                    .or_else(dirs::home_dir);
                if let Some(home) = home {
                    if let Err(e) = discoverer.write_back(
                        &home,
                        &tokens.access_token,
                        &tokens.refresh_token,
                        tokens.expires_at_ms,
                    ) {
                        warn!(
                            target: "cortex::llm::credential_pool",
                            source = name.as_str(),
                            credential = id,
                            error = %e,
                            "oauth refresh: write-back to source file failed — \
                             next daemon reload may use a stale refresh_token"
                        );
                    }
                }
            }
        }
    }
}

/// True when `expires_at_ms` is missing or within 60 seconds of `now_ms`.
/// Missing expiry treated as "needs refresh" so a legacy persisted row with
/// stale state forces one round-trip rather than gambling on a dead token.
fn needs_refresh(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        None => true,
        Some(exp) => exp <= now_ms + 60_000,
    }
}

// ── LeaseHandle ───────────────────────────────────────────────────────────

/// RAII guard for an active lease. Drop decrements the per-credential
/// counter; never decrements below zero.
pub struct LeaseHandle {
    pool: Arc<Mutex<PoolInner>>,
    credential_id: String,
}

impl LeaseHandle {
    /// The leased credential's id.
    pub fn id(&self) -> &str {
        &self.credential_id
    }

    /// Snapshot of the leased credential. Returns `None` only if the
    /// credential was concurrently removed from the pool (unusual).
    pub fn credential(&self) -> Option<PooledCredential> {
        let g = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.creds.iter().find(|c| c.id == self.credential_id).cloned()
    }

    /// Plaintext API key. Returns `None` for a tombstoned credential.
    pub fn key(&self) -> Option<String> {
        self.credential()
            .filter(|c| !c.key.is_empty())
            .map(|c| c.key)
    }

    /// Per-credential `base_url` override (set by `CredentialSource::Custom`).
    /// `None` when the credential uses the provider's default URL. The HTTP
    /// path consults this before falling back to `LlmConfig.base_url` →
    /// `Provider::base_url()`.
    pub fn base_url(&self) -> Option<String> {
        self.credential().and_then(|c| c.base_url)
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        let mut g = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = g.active_leases.get_mut(&self.credential_id) {
            if *count <= 1 {
                g.active_leases.remove(&self.credential_id);
            } else {
                *count -= 1;
            }
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────────────────

#[derive(Debug)]
struct PersistedRow {
    fingerprint: String,
    id: String,
    label: String,
    priority: u32,
    source_json: String,
    use_count: u64,
    last_status: String,
    last_status_at_ms: Option<i64>,
    last_error_code: Option<u16>,
    last_error_reason: Option<String>,
    last_error_message: Option<String>,
    last_error_reset_at_ms: Option<i64>,
    removed_at_ms: Option<i64>,
    auth_type: AuthType,
    expires_at_ms: Option<i64>,
    base_url: Option<String>,
    extra_json: Option<String>,
}

fn reconcile_or_new(
    persisted: &HashMap<String, PersistedRow>,
    fingerprint: String,
    key: String,
    label: String,
    source: CredentialSource,
    priority: u32,
    now_ms: i64,
) -> PooledCredential {
    if let Some(row) = persisted.get(&fingerprint) {
        // Auth-class removals (401/403) are sticky: even if the source is still
        // present in config/env, the credential stays tombstoned until the
        // operator runs `borg auth reset` or removes/replaces the source.
        PooledCredential {
            id: row.id.clone(),
            label: row.label.clone(),
            priority,
            source: serde_json::from_str(&row.source_json).unwrap_or(source),
            key: if row.removed_at_ms.is_some() {
                String::new()
            } else {
                key
            },
            fingerprint,
            use_count: row.use_count,
            last_status: CredentialStatus::from_db(&row.last_status),
            last_status_at_ms: row.last_status_at_ms,
            last_error_code: row.last_error_code,
            last_error_reason: row.last_error_reason.clone(),
            last_error_message: row.last_error_message.clone(),
            last_error_reset_at_ms: row.last_error_reset_at_ms,
            removed_at_ms: row.removed_at_ms,
            auth_type: row.auth_type,
            // Refresh tokens never round-trip through the seed path —
            // OAuth credentials seed via the credential-source discoverers
            // which carry refresh_token explicitly, not through fingerprint
            // reconciliation.
            refresh_token: None,
            expires_at_ms: row.expires_at_ms,
            base_url: row.base_url.clone(),
            extra_json: row.extra_json.clone(),
            rate_limit: None,
        }
    } else {
        PooledCredential {
            id: random_id(),
            label,
            priority,
            source,
            key,
            fingerprint,
            use_count: 0,
            last_status: CredentialStatus::Ok,
            last_status_at_ms: Some(now_ms),
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at_ms: None,
            removed_at_ms: None,
            auth_type: AuthType::ApiKey,
            refresh_token: None,
            expires_at_ms: None,
            base_url: None,
            extra_json: None,
            rate_limit: None,
        }
    }
}

/// OAuth/custom-endpoint variant of [`reconcile_or_new`]. Preserves persisted
/// state (use_count, cooldown, suppression) but always refreshes
/// `key` / `refresh_token` / `expires_at_ms` / `base_url` / `extra_json` from
/// the live discovery payload — those secrets aren't persisted to DB.
fn reconcile_or_new_oauth(
    persisted: &HashMap<String, PersistedRow>,
    fingerprint: String,
    disc: crate::llm::credential_sources::DiscoveredCredential,
    source: CredentialSource,
    auth_type: AuthType,
    priority: u32,
    now_ms: i64,
) -> PooledCredential {
    if let Some(row) = persisted.get(&fingerprint) {
        PooledCredential {
            id: row.id.clone(),
            label: row.label.clone(),
            priority,
            source: serde_json::from_str(&row.source_json).unwrap_or(source),
            key: if row.removed_at_ms.is_some() {
                String::new()
            } else {
                disc.access_token
            },
            fingerprint,
            use_count: row.use_count,
            last_status: CredentialStatus::from_db(&row.last_status),
            last_status_at_ms: row.last_status_at_ms,
            last_error_code: row.last_error_code,
            last_error_reason: row.last_error_reason.clone(),
            last_error_message: row.last_error_message.clone(),
            last_error_reset_at_ms: row.last_error_reset_at_ms,
            removed_at_ms: row.removed_at_ms,
            auth_type,
            refresh_token: if row.removed_at_ms.is_some() {
                None
            } else {
                disc.refresh_token
            },
            expires_at_ms: disc.expires_at_ms.or(row.expires_at_ms),
            base_url: disc.base_url.or_else(|| row.base_url.clone()),
            extra_json: disc.extra_json.or_else(|| row.extra_json.clone()),
            rate_limit: None,
        }
    } else {
        PooledCredential {
            id: random_id(),
            label: disc.label,
            priority,
            source,
            key: disc.access_token,
            fingerprint,
            use_count: 0,
            last_status: CredentialStatus::Ok,
            last_status_at_ms: Some(now_ms),
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at_ms: None,
            removed_at_ms: None,
            auth_type,
            refresh_token: disc.refresh_token,
            expires_at_ms: disc.expires_at_ms,
            base_url: disc.base_url,
            extra_json: disc.extra_json,
            rate_limit: None,
        }
    }
}

/// Operations that can be fired-and-forgotten onto the tokio runtime by the
/// pool's sync API. Each variant carries everything the async helper needs to
/// open a short-lived libsql connection and run the matching SQL.
#[allow(clippy::large_enum_variant)]
enum PersistOp {
    Upsert(PooledCredential),
    Suppress {
        source: CredentialSource,
        now_ms: i64,
    },
    Unsuppress {
        source_kind: String,
        source_value: String,
    },
}

fn spawn_persist_task(db_path: std::path::PathBuf, provider: String, op: PersistOp) {
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            // No runtime context — typically a CLI entrypoint reached the pool
            // before borgd's tokio runtime spun up. Log and skip; the pool's
            // in-memory state still reflects the new value and the next load
            // reconciles via env/config.
            warn!(
                target: "cortex::llm::credential_pool",
                provider = %provider,
                "credential pool: no tokio runtime; persist skipped"
            );
            return;
        }
    };
    handle.spawn(async move {
        let res = match op {
            PersistOp::Upsert(cred) => persist_one(&db_path, &provider, &cred).await,
            PersistOp::Suppress { source, now_ms } => {
                persist_suppression(&db_path, &provider, &source, now_ms).await
            }
            PersistOp::Unsuppress {
                source_kind,
                source_value,
            } => delete_suppression(&db_path, &provider, &source_kind, &source_value).await,
        };
        if let Err(e) = res {
            warn!(
                target: "cortex::llm::credential_pool",
                provider = %provider,
                error = %e,
                "credential pool: persist failed"
            );
        }
    });
}

async fn open_short_conn(db_path: &std::path::Path) -> Result<libsql::Connection> {
    let path_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 db path: {}", db_path.display()))?;
    let db = libsql::Builder::new_local(path_str)
        .build()
        .await
        .with_context(|| format!("libsql build at {}", db_path.display()))?;
    let conn = db
        .connect()
        .with_context(|| format!("libsql connect at {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .context("set busy_timeout")?;
    Ok(conn)
}

async fn upsert_row(conn: &libsql::Connection, provider: &str, c: &PooledCredential) -> Result<()> {
    let source_json = serde_json::to_string(&c.source).context("serialize source")?;
    let auth_type_db = match c.auth_type {
        AuthType::ApiKey => "api_key",
        AuthType::OAuth => "oauth",
    };
    conn.execute(
        "INSERT INTO credential_pool
             (provider, fingerprint, id, label, priority, source_json, use_count,
              last_status, last_status_at_ms, last_error_code, last_error_reason,
              last_error_message, last_error_reset_at_ms, removed_at_ms,
              auth_type, expires_at_ms, base_url, extra_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18)
         ON CONFLICT(provider, fingerprint) DO UPDATE SET
             id = excluded.id,
             label = excluded.label,
             priority = excluded.priority,
             source_json = excluded.source_json,
             use_count = excluded.use_count,
             last_status = excluded.last_status,
             last_status_at_ms = excluded.last_status_at_ms,
             last_error_code = excluded.last_error_code,
             last_error_reason = excluded.last_error_reason,
             last_error_message = excluded.last_error_message,
             last_error_reset_at_ms = excluded.last_error_reset_at_ms,
             removed_at_ms = excluded.removed_at_ms,
             auth_type = excluded.auth_type,
             expires_at_ms = excluded.expires_at_ms,
             base_url = excluded.base_url,
             extra_json = excluded.extra_json",
        params!(
            provider,
            c.fingerprint.clone(),
            c.id.clone(),
            c.label.clone(),
            c.priority as i64,
            source_json,
            c.use_count as i64,
            c.last_status.as_db(),
            c.last_status_at_ms,
            c.last_error_code.map(|x| x as i64),
            c.last_error_reason.clone(),
            c.last_error_message.clone(),
            c.last_error_reset_at_ms,
            c.removed_at_ms,
            auth_type_db,
            c.expires_at_ms,
            c.base_url.clone(),
            c.extra_json.clone(),
        ),
    )
    .await?;
    Ok(())
}

async fn persist_one(
    db_path: &std::path::Path,
    provider: &str,
    c: &PooledCredential,
) -> Result<()> {
    let conn = open_short_conn(db_path).await?;
    upsert_row(&conn, provider, c).await
}

async fn persist_suppression(
    db_path: &std::path::Path,
    provider: &str,
    source: &CredentialSource,
    now_ms: i64,
) -> Result<()> {
    let conn = open_short_conn(db_path).await?;
    conn.execute(
        "INSERT INTO credential_suppressions (provider, source_kind, source_value, suppressed_at_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(provider, source_kind, source_value) DO UPDATE SET
             suppressed_at_ms = excluded.suppressed_at_ms",
        params!(provider, source.kind_db(), source.value_db(), now_ms),
    )
    .await?;
    Ok(())
}

async fn delete_suppression(
    db_path: &std::path::Path,
    provider: &str,
    source_kind: &str,
    source_value: &str,
) -> Result<()> {
    let conn = open_short_conn(db_path).await?;
    conn.execute(
        "DELETE FROM credential_suppressions
         WHERE provider = ?1 AND source_kind = ?2 AND source_value = ?3",
        params!(provider, source_kind, source_value),
    )
    .await?;
    Ok(())
}

// ── Small utilities ───────────────────────────────────────────────────────

/// 16-hex truncated BLAKE3 of the plaintext key. Stable identifier;
/// reveals nothing about the key (one-way + truncation).
pub fn fingerprint(key: &str) -> String {
    let hash = blake3::hash(key.as_bytes());
    let full = hash.to_hex();
    full[..16].to_string()
}

/// Validate that `url` is safe to use as a per-credential `base_url` override.
///
/// Accepts: `https://<any-host>` and `http://<loopback-host>`. Rejects
/// anything else — schemes other than http(s) (file/ftp/gopher), plain
/// `http://` to non-loopback hosts (clear-text bearer over the wire), or
/// strings that don't parse as a URL.
///
/// The threat model: anyone with local-user write access to
/// `~/.qwen/oauth_creds.json` (or a future operator-supplied custom-endpoint
/// file) could otherwise redirect the bearer token to an attacker host on
/// next pool reload. They already have code-exec at that point, but the
/// validation closes the obvious "swap one URL string" vector and surfaces
/// a clear error to the operator instead of silently exfiltrating tokens.
pub fn is_safe_base_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match parsed.scheme() {
        "https" => true,
        "http" => parsed
            .host_str()
            .map(|h| {
                h == "localhost"
                    || h == "127.0.0.1"
                    || h == "[::1]"
                    || h == "::1"
                    || h.starts_with("127.")
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn random_id() -> String {
    let mut rng = rand::rng();
    let mut buf = [0u8; 4];
    rng.fill(&mut buf);
    // 8-hex (4 bytes ≈ 4.3B values). Matches the `borg auth list` ID column
    // truncate and gives `resolve_target`'s 4-hex-prefix matcher real headroom.
    const TBL: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(8);
    for b in buf {
        out.push(TBL[(b >> 4) as usize] as char);
        out.push(TBL[(b & 0xf) as usize] as char);
    }
    out
}

fn truncate_error_msg(s: &str) -> String {
    // Redact first — upstream error bodies occasionally echo the bearer
    // token tail back, and `last_error_message` ends up on operator
    // terminals via `borg auth list` (and into shell history).
    let redacted = crate::security::secrets::redact_secrets(s);
    if redacted.len() <= ERROR_MESSAGE_MAX_LEN {
        return redacted;
    }
    let mut end = ERROR_MESSAGE_MAX_LEN;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &redacted[..end])
}

/// Build a [`PoolLoadConfig`] from the primary `LlmConfig` plus the resolved
/// provider. `api_keys` is resolved by the caller (which has the `SecretRef`
/// chain) and passed in as already-plaintext entries.
pub fn pool_load_config_for_provider(
    provider: Provider,
    strategy: CredentialStrategy,
    cooldown_secs: u64,
    max_concurrent_per_credential: u32,
    config_entries: Vec<ConfigEntry>,
) -> PoolLoadConfig {
    let mut env_vars = vec![provider.default_env_var().to_string()];
    if matches!(provider, Provider::Anthropic) {
        // Anthropic users sometimes set both ANTHROPIC_API_KEY and
        // CLAUDE_API_KEY; surface both so a typo doesn't dead-end the user.
        for v in ["ANTHROPIC_AUTH_TOKEN", "CLAUDE_API_KEY"] {
            if !env_vars.contains(&v.to_string()) {
                env_vars.push(v.to_string());
            }
        }
    }
    PoolLoadConfig {
        strategy,
        cooldown_secs,
        max_concurrent_per_credential,
        proactive_rate_limit_threshold_pct: 0,
        proactive_rate_limit_min_window_secs: 0,
        config_entries,
        env_vars,
        oauth_home: None,
    }
}

/// Same as [`pool_load_config_for_provider`], but also threads the proactive
/// rate-limit knobs. New callers should use this; the no-suffix variant
/// remains for backward compatibility (it disables proactive avoidance).
pub fn pool_load_config_for_provider_with_proactive(
    provider: Provider,
    strategy: CredentialStrategy,
    cooldown_secs: u64,
    max_concurrent_per_credential: u32,
    proactive_rate_limit_threshold_pct: u8,
    proactive_rate_limit_min_window_secs: u64,
    config_entries: Vec<ConfigEntry>,
) -> PoolLoadConfig {
    let mut cfg = pool_load_config_for_provider(
        provider,
        strategy,
        cooldown_secs,
        max_concurrent_per_credential,
        config_entries,
    );
    cfg.proactive_rate_limit_threshold_pct = proactive_rate_limit_threshold_pct;
    cfg.proactive_rate_limit_min_window_secs = proactive_rate_limit_min_window_secs;
    cfg
}

/// Convenience: build a pool from already-resolved plaintext keys. Skips the
/// `api_keys`-SecretRef-resolution dance — the caller already did it (see
/// `Config::resolve_api_keys`). Best-effort: returns `None` if the DB path
/// can't be resolved or the load fails. Logs at WARN on failure so prod
/// regressions are observable.
pub async fn try_build_from_keys(
    provider: Provider,
    plaintext_keys: Vec<String>,
    strategy: CredentialStrategy,
    cooldown_secs: u64,
    max_concurrent_per_credential: u32,
    proactive_rate_limit_threshold_pct: u8,
    proactive_rate_limit_min_window_secs: u64,
) -> Option<Arc<CredentialPool>> {
    let db_path = match crate::config::Config::db_path() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                target: "cortex::llm::credential_pool",
                error = %e,
                "credential pool: cannot resolve db path — falling through to legacy rotation"
            );
            return None;
        }
    };
    let db = match Database::open_at(&db_path).await {
        Ok(d) => d,
        Err(e) => {
            warn!(
                target: "cortex::llm::credential_pool",
                error = %e,
                "credential pool: cannot open DB — falling through to legacy rotation"
            );
            return None;
        }
    };
    let config_entries: Vec<ConfigEntry> = plaintext_keys
        .into_iter()
        .enumerate()
        .map(|(i, k)| ConfigEntry {
            key: k,
            label: None,
            config_name: format!("llm.api_keys[{i}]"),
        })
        .collect();
    let cfg = pool_load_config_for_provider_with_proactive(
        provider,
        strategy,
        cooldown_secs,
        max_concurrent_per_credential,
        proactive_rate_limit_threshold_pct,
        proactive_rate_limit_min_window_secs,
        config_entries,
    );
    match CredentialPool::load(&db, &db_path, provider, cfg).await {
        Ok(pool) => Some(pool),
        Err(e) => {
            warn!(
                target: "cortex::llm::credential_pool",
                error = %e,
                "credential pool: load failed — falling through to legacy rotation"
            );
            None
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

// TODO: needs async DB harness
#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[derive(Debug)]
    struct TestClock {
        now: AtomicI64,
    }
    impl TestClock {
        fn new(start_ms: i64) -> Arc<Self> {
            Arc::new(Self {
                now: AtomicI64::new(start_ms),
            })
        }
        fn advance(&self, d: Duration) {
            self.now.fetch_add(d.as_millis() as i64, Ordering::SeqCst);
        }
    }
    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
    }

    fn open_test_db() -> (Database, std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::open_at(&path).unwrap();
        (db, path, dir)
    }

    fn cfg_with(keys: &[(&str, &str)]) -> PoolLoadConfig {
        let entries = keys
            .iter()
            .enumerate()
            .map(|(i, (lbl, k))| ConfigEntry {
                key: (*k).into(),
                label: Some((*lbl).into()),
                config_name: format!("llm.api_keys[{i}]"),
            })
            .collect();
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            max_concurrent_per_credential: 1,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: entries,
            env_vars: vec![],
            oauth_home: None,
        }
    }

    // ── A1. Construction + load + seeding ──

    #[test]
    fn load_empty_config_no_env_yields_empty_pool() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            PoolLoadConfig {
                strategy: CredentialStrategy::FillFirst,
                cooldown_secs: 60,
                max_concurrent_per_credential: 1,
                proactive_rate_limit_threshold_pct: 0,
                proactive_rate_limit_min_window_secs: 0,
                config_entries: vec![],
                env_vars: vec!["BORG_TEST_NEVER_SET_XYZ".into()],
                oauth_home: None,
            },
        )
        .unwrap();
        assert!(!pool.has_credentials());
        assert!(pool.pick().is_none());
    }

    #[test]
    fn load_with_one_manual_key_seeds_one_entry() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("alpha", "k1")]),
        )
        .unwrap();
        assert_eq!(pool.entries().len(), 1);
        assert_eq!(pool.entries()[0].label, "alpha");
    }

    #[test]
    fn load_dedupes_same_key_under_two_refs() {
        // Same plaintext key under two `SecretRef` entries collapses to one.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "samekey"), ("b", "samekey")]),
        )
        .unwrap();
        assert_eq!(pool.entries().len(), 1);
    }

    #[test]
    fn load_keeps_distinct_keys_separate() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        assert_eq!(pool.entries().len(), 2);
    }

    // ── A2. peek / pick per strategy ──

    #[test]
    fn fill_first_sticks_to_head() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        let first = pool.pick().unwrap();
        let second = pool.pick().unwrap();
        let third = pool.pick().unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.id, third.id);
    }

    #[test]
    fn round_robin_rotates() {
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
        cfg.strategy = CredentialStrategy::RoundRobin;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let mut counts: HashMap<String, u32> = HashMap::new();
        for _ in 0..9 {
            let c = pool.pick().unwrap();
            *counts.entry(c.id).or_insert(0) += 1;
        }
        for (_, v) in counts {
            assert_eq!(v, 3);
        }
    }

    #[test]
    fn least_used_picks_lowest_count() {
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
        cfg.strategy = CredentialStrategy::LeastUsed;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..3 {
            seen.insert(pool.pick().unwrap().id);
        }
        // First three picks must touch every key.
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn random_spreads_picks() {
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]);
        cfg.strategy = CredentialStrategy::Random;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let mut counts: HashMap<String, u32> = HashMap::new();
        for _ in 0..600 {
            *counts.entry(pool.pick().unwrap().id).or_insert(0) += 1;
        }
        for (_, v) in counts {
            assert!(v > 100, "expected even-ish spread, got {v}");
        }
    }

    // ── A3. report_failure transitions ──

    #[test]
    fn rate_limit_cools_entry() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1")]),
            clock.clone(),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
        assert!(pool.is_exhausted());
        clock.advance(Duration::from_secs(61));
        assert!(!pool.is_exhausted());
    }

    #[test]
    fn auth_failure_removes_permanently() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
        let entries = pool.entries();
        let removed = entries.iter().find(|c| c.id == cred.id).unwrap();
        assert!(removed.is_removed());
        // Pool still has the other key.
        assert!(pool.has_available());
    }

    #[test]
    fn auth_permanent_also_removes() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::AuthPermanent,
            Some(403),
            None,
            None,
        );
        assert!(pool.entries()[0].is_removed());
    }

    #[test]
    fn transport_errors_dont_mutate_pool() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        let baseline_use = pool.entries()[0].use_count;
        for r in [
            FailoverReason::Overloaded,
            FailoverReason::Timeout,
            FailoverReason::Format,
            FailoverReason::ContextOverflow,
            FailoverReason::PayloadTooLarge,
            FailoverReason::ModelNotFound,
            FailoverReason::Unknown,
        ] {
            pool.report_failure(&cred.id, r, None, None, None);
        }
        let after = &pool.entries()[0];
        assert_eq!(after.use_count, baseline_use);
        assert!(!after.is_removed());
        assert!(matches!(after.last_status, CredentialStatus::Ok));
    }

    #[test]
    fn retry_after_overrides_default_cooldown() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1")]),
            clock.clone(),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            Some(429),
            Some(Duration::from_secs(600)),
            None,
        );
        let until = pool.entries()[0].last_error_reset_at_ms.unwrap();
        // Tight equality, not `>=`: the explicit 600s retry_after must drive
        // the cooldown exactly. A regression that fell back to the configured
        // 60s default would land at 1_060_000 (caught); a runaway clamp to 24h
        // would land at 1_000_000 + 86_400_000 (also caught).
        assert_eq!(until, 1_000_000 + 600_000);
    }

    #[test]
    fn cooldown_extends_never_shortens() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1")]),
            clock.clone(),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(600)),
            None,
        );
        let long = pool.entries()[0].last_error_reset_at_ms.unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(10)),
            None,
        );
        let after = pool.entries()[0].last_error_reset_at_ms.unwrap();
        assert_eq!(after, long, "shorter cooldown must not win");
    }

    #[test]
    fn report_failure_on_removed_is_noop() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
        let removed_at = pool.entries()[0].removed_at_ms;
        pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
        // No state change after the removal.
        assert_eq!(pool.entries()[0].removed_at_ms, removed_at);
        assert!(pool.entries()[0].last_error_reset_at_ms.is_none());
    }

    #[test]
    fn error_message_truncates_at_cap() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        let big = "x".repeat(ERROR_MESSAGE_MAX_LEN * 3);
        pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, Some(&big));
        let msg = pool.entries()[0].last_error_message.clone().unwrap();
        assert!(msg.contains("truncated"));
        assert!(msg.len() < big.len());
    }

    // ── A4. report_success ──

    #[test]
    fn success_clears_cooldown_and_error_state() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1")]),
            clock,
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::RateLimit, Some(429), None, None);
        assert!(pool.is_exhausted());
        pool.report_success(&cred.id);
        let after = &pool.entries()[0];
        assert!(matches!(after.last_status, CredentialStatus::Ok));
        assert!(after.last_error_code.is_none());
        assert!(after.last_error_reset_at_ms.is_none());
        assert!(pool.has_available());
    }

    #[test]
    fn success_on_removed_is_noop() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
        pool.report_success(&cred.id);
        assert!(pool.entries()[0].is_removed());
    }

    // ── A5. Exhaustion / recovery ──

    #[test]
    fn single_key_exhausts() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1")]),
            clock.clone(),
        )
        .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
        assert!(pool.is_exhausted());
        clock.advance(Duration::from_secs(61));
        assert!(!pool.is_exhausted());
    }

    #[test]
    fn next_recovery_at_returns_earliest() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
            clock,
        )
        .unwrap();
        let entries = pool.entries();
        pool.report_failure(
            &entries[0].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(100)),
            None,
        );
        pool.report_failure(
            &entries[1].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(50)),
            None,
        );
        let next = pool.next_recovery_at_ms().unwrap();
        // Earliest = entry[1] at +50s.
        assert!(next < 1_000_000 + 60_000);
    }

    // ── A6. pick_force ──

    #[test]
    fn pick_force_returns_soonest_recovery() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
            clock,
        )
        .unwrap();
        let entries = pool.entries();
        pool.report_failure(
            &entries[0].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(100)),
            None,
        );
        pool.report_failure(
            &entries[1].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(50)),
            None,
        );
        pool.report_failure(
            &entries[2].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(200)),
            None,
        );
        let chosen = pool.pick_force().unwrap();
        assert_eq!(chosen.id, entries[1].id);
    }

    #[test]
    fn pick_force_bumps_use_count() {
        // Regression for the "salvage shows used 0×" bug: pick_force is the
        // last-gasp path against a cooled key, and its accounting feeds both
        // least_used selection and operator-visible `borg auth list` columns.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
        let before = pool.entries()[0].use_count;
        let _ = pool.pick_force().unwrap();
        let after = pool.entries()[0].use_count;
        assert_eq!(after, before + 1);
    }

    #[test]
    fn pick_force_returns_none_when_all_removed() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        for e in pool.entries() {
            pool.report_failure(&e.id, FailoverReason::Auth, Some(401), None, None);
        }
        assert!(pool.pick_force().is_none());
    }

    // ── A8. reset_cooldowns ──

    #[test]
    fn reset_cooldowns_clears_exhausted_only() {
        let (db, path, _g) = open_test_db();
        let clock = TestClock::new(1_000_000);
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
            clock,
        )
        .unwrap();
        let entries = pool.entries();
        pool.report_failure(&entries[0].id, FailoverReason::RateLimit, None, None, None);
        pool.report_failure(&entries[1].id, FailoverReason::Auth, Some(401), None, None);
        let cleared = pool.reset_cooldowns();
        assert_eq!(cleared, 1);
        // The removed entry stays removed.
        let after = pool.entries();
        assert!(after
            .iter()
            .find(|c| c.id == entries[1].id)
            .unwrap()
            .is_removed());
        // The cooled entry is now Ok.
        let zero = after.iter().find(|c| c.id == entries[0].id).unwrap();
        assert!(matches!(zero.last_status, CredentialStatus::Ok));
    }

    #[test]
    fn reset_cooldowns_idempotent() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::RateLimit, None, None, None);
        let n1 = pool.reset_cooldowns();
        let n2 = pool.reset_cooldowns();
        assert_eq!(n1, 1);
        assert_eq!(n2, 0);
    }

    // ── A9. Leasing ──

    #[test]
    fn acquire_lease_returns_below_cap_keys() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2"), ("c", "k3")]),
        )
        .unwrap();
        let l1 = pool.acquire_lease(None).unwrap();
        let l2 = pool.acquire_lease(None).unwrap();
        let l3 = pool.acquire_lease(None).unwrap();
        let ids: HashSet<String> = [&l1, &l2, &l3].iter().map(|l| l.id().to_string()).collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn lease_drop_decrements_counter() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        {
            let _l = pool.acquire_lease(None).unwrap();
        }
        // After drop, a fresh lease still succeeds (counter decremented).
        let _l2 = pool.acquire_lease(None).unwrap();
    }

    #[test]
    fn acquire_lease_when_all_capped_still_returns_one() {
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1")]);
        cfg.max_concurrent_per_credential = 1;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let _l1 = pool.acquire_lease(None).unwrap();
        let l2 = pool.acquire_lease(None);
        // Single key + cap=1 still returns a lease (least-leased fallback),
        // letting concurrent requests degrade gracefully rather than block.
        assert!(l2.is_some());
    }

    #[test]
    fn acquire_lease_unknown_id_returns_none() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        assert!(pool.acquire_lease(Some("bogus")).is_none());
    }

    #[test]
    fn credential_source_round_trips_every_variant_through_serde() {
        // Schema contract: every CredentialSource variant must serialize and
        // deserialize losslessly through the same `source_json` column used by
        // `upsert_row` and the `load` SELECT. A typo in `#[serde(rename)]` for
        // any new variant would silently rewrite the persistence format,
        // stranding existing rows.
        let cases = vec![
            CredentialSource::Manual,
            CredentialSource::Env {
                var: "OPENROUTER_API_KEY".into(),
            },
            CredentialSource::Config {
                name: "llm.api_keys[0]".into(),
            },
            CredentialSource::OAuth {
                name: "claude_code".into(),
            },
            CredentialSource::Custom {
                name: "local-vllm".into(),
                base_url: "http://127.0.0.1:8000".into(),
            },
        ];
        for src in cases {
            let s = serde_json::to_string(&src).expect("serialize");
            let back: CredentialSource = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(back, src, "round-trip failed for {src:?}: {s}");
        }
    }

    #[test]
    fn credential_pool_table_has_phase_one_columns() {
        // Migration contract: a fresh DB must expose the five new columns added
        // for OAuth + custom-endpoint parity. Catches the case where someone
        // adds a Rust field but forgets the SQL — load would then fail at
        // SELECT-time and silently fall through to the legacy rotation path.
        let (db, _path, _g) = open_test_db();
        let conn = db.conn();
        let mut stmt = conn.prepare("PRAGMA table_info(credential_pool)").unwrap();
        let cols: HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for expected in [
            "auth_type",
            "expires_at_ms",
            "base_url",
            "extra_json",
            // sanity-check one preexisting column to confirm the PRAGMA path
            // actually returned schema and not an empty set.
            "fingerprint",
        ] {
            assert!(
                cols.contains(expected),
                "missing column `{expected}` in credential_pool: have {cols:?}"
            );
        }
    }

    #[test]
    fn max_concurrent_cap_round_trips_through_config_and_clamps() {
        // The cap must be read from PoolLoadConfig (not hard-coded) and the
        // `.max(1)` clamp at load time keeps cap=0 from bricking the pool —
        // every available credential would otherwise be over-cap forever.
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1")]);
        cfg.max_concurrent_per_credential = 5;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        assert_eq!(pool.max_concurrent_per_credential(), 5);

        let (db2, path2, _g2) = open_test_db();
        let mut cfg2 = cfg_with(&[("a", "k1")]);
        cfg2.max_concurrent_per_credential = 0;
        let pool2 = CredentialPool::load(&db2, &path2, Provider::OpenRouter, cfg2).unwrap();
        assert_eq!(
            pool2.max_concurrent_per_credential(),
            1,
            "cap=0 must clamp to 1"
        );
    }

    #[test]
    fn max_concurrent_cap_allows_multiple_leases_per_key_when_under_cap() {
        // cap=2 + single key: two concurrent leases must both succeed against
        // the same id (cap-aware path stays in the below_cap branch). If the
        // cap were silently 1, the second lease would still succeed via the
        // graceful-degradation fallback — but a third lease would not be
        // distinguishable. So we also verify the per-key counter decrements
        // on drop by re-leasing after one drop.
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1")]);
        cfg.max_concurrent_per_credential = 2;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let l1 = pool.acquire_lease(None).unwrap();
        let l2 = pool.acquire_lease(None).unwrap();
        assert_eq!(l1.id(), l2.id(), "single key — both leases land on it");
        let id1 = l1.id().to_string();
        drop(l1);
        let l3 = pool.acquire_lease(None).unwrap();
        assert_eq!(l3.id(), id1, "post-drop lease re-uses the same key");
    }

    #[test]
    fn max_concurrent_cap_distributes_across_keys_under_cap() {
        // cap=2 + two keys: first two leases must land on *different* keys.
        // The tiebreak `(active_leases, priority)` prefers the unleased key
        // over re-leasing the first even though both are below cap. Four
        // leases distribute evenly (2 per key). If anyone breaks the
        // below-cap preference, the first key gets all leases.
        let (db, path, _g) = open_test_db();
        let mut cfg = cfg_with(&[("a", "k1"), ("b", "k2")]);
        cfg.max_concurrent_per_credential = 2;
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
        let l1 = pool.acquire_lease(None).unwrap();
        let l2 = pool.acquire_lease(None).unwrap();
        assert_ne!(
            l1.id(),
            l2.id(),
            "below-cap preference must spread before doubling up"
        );
        let l3 = pool.acquire_lease(None).unwrap();
        let l4 = pool.acquire_lease(None).unwrap();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for id in [l1.id(), l2.id(), l3.id(), l4.id()] {
            *counts.entry(id.to_string()).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 2, "both keys leased");
        assert!(
            counts.values().all(|&n| n == 2),
            "even distribution under cap: got {counts:?}"
        );
    }

    // ── A10. add_entry / remove_index / resolve_target ──

    #[test]
    fn add_entry_appends_and_returns_idempotent_on_dup() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let added = pool.add_entry("k2".into(), "added".into(), CredentialSource::Manual);
        assert_eq!(pool.entries().len(), 2);
        let again = pool.add_entry("k2".into(), "ignored".into(), CredentialSource::Manual);
        assert_eq!(again.id, added.id);
        assert_eq!(pool.entries().len(), 2);
    }

    #[test]
    fn add_entry_full_preserves_oauth_metadata_across_reload() {
        // Regression for the bug where `pair_credential` used `add_entry` and
        // silently downgraded OAuth credentials to API keys (no auth_type=oauth,
        // no refresh_token, no expires_at, no base_url). Refresh would never
        // fire and the credential would die when the access token expired.
        //
        // Asserts on the reloaded pool, not the in-memory return value — the
        // real win is that auth_type/expires_at survive SQLite round-trip.
        // (refresh_token is intentionally NOT persisted to DB; the rotated
        // value lives in the source file via `write_back`.)
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"access-token","refreshToken":"refresh-token","expiresAt":9999999999999}}"#,
        )
        .unwrap();
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        let cred = pool.add_entry_full(
            "access-token".into(),
            "claude-code:abc123".into(),
            CredentialSource::OAuth {
                name: "claude_code".into(),
            },
            AuthType::OAuth,
            Some("refresh-token".into()),
            Some(9_999_999_999_999),
            None,
            None,
        );
        assert_eq!(cred.auth_type, AuthType::OAuth);
        assert_eq!(cred.refresh_token.as_deref(), Some("refresh-token"));

        // Drop the live pool and reload — proves the OAuth metadata
        // round-trips through SQLite. Without this assertion the test would
        // pass even if `upsert_row` silently dropped auth_type=oauth.
        drop(pool);
        let pool2 = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        let entries = pool2.entries();
        let reloaded = entries
            .iter()
            .find(|c| c.id == cred.id)
            .expect("entry must survive reload");
        assert_eq!(
            reloaded.auth_type,
            AuthType::OAuth,
            "auth_type must round-trip — otherwise refresh path takes the API-key fast-path"
        );
        assert_eq!(reloaded.expires_at_ms, Some(9_999_999_999_999));
        assert!(matches!(
            reloaded.source,
            CredentialSource::OAuth { ref name } if name == "claude_code"
        ));
    }

    #[test]
    fn add_entry_full_with_custom_base_url_round_trips() {
        // Custom-endpoint credentials carry a per-credential `base_url` that
        // the HTTP path consults at request time. The pool must preserve it
        // through `add_entry_full` so `LeaseHandle::base_url()` returns it.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::Custom,
            oauth_cfg(std::path::Path::new("/nonexistent")),
        )
        .unwrap();
        let cred = pool.add_entry_full(
            "qwen-api-key".into(),
            "qwen-cli".into(),
            CredentialSource::Custom {
                name: "qwen_cli".into(),
                base_url: "https://dashscope.aliyuncs.com/api/v1".into(),
            },
            AuthType::ApiKey,
            None,
            None,
            Some("https://dashscope.aliyuncs.com/api/v1".into()),
            None,
        );
        assert_eq!(
            cred.base_url.as_deref(),
            Some("https://dashscope.aliyuncs.com/api/v1")
        );
        // Lease should surface the override.
        let lease = pool.acquire_lease(Some(&cred.id)).unwrap();
        assert_eq!(
            lease.base_url().as_deref(),
            Some("https://dashscope.aliyuncs.com/api/v1")
        );
    }

    #[test]
    fn is_safe_base_url_blocks_dangerous_schemes() {
        // Defends against an attacker who can write `~/.qwen/oauth_creds.json`
        // pointing the bearer at their host. https is open (TLS gates the
        // wire), http requires loopback (clear-text is fine on localhost),
        // everything else fails closed.
        assert!(is_safe_base_url("https://api.example.com/v1"));
        assert!(is_safe_base_url("http://localhost:8080/v1"));
        assert!(is_safe_base_url("http://127.0.0.1:11434/v1"));
        assert!(is_safe_base_url("http://[::1]/v1"));
        assert!(!is_safe_base_url("http://evil.com/exfil"));
        assert!(!is_safe_base_url("file:///etc/passwd"));
        assert!(!is_safe_base_url("ftp://files.example.com/"));
        assert!(!is_safe_base_url("javascript:alert(1)"));
        assert!(!is_safe_base_url("not-a-url"));
        assert!(!is_safe_base_url(""));
    }

    #[test]
    fn add_entry_resurrects_tombstoned_fingerprint() {
        // Regression: re-adding a previously-removed key used to silently
        // return the tombstone — operator saw "Added credential…" but the
        // pool never used the key. The resurrection path must clear the
        // tombstone, restore the plaintext, and delete the suppression row.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let added = pool.add_entry("k2".into(), "personal".into(), CredentialSource::Manual);
        let _removed = pool.remove_index(2).unwrap();
        assert!(pool.entries()[1].is_removed());

        let resurrected = pool.add_entry("k2".into(), "ignored".into(), CredentialSource::Manual);
        assert_eq!(resurrected.id, added.id, "same entry resurrected, not new");
        assert!(!resurrected.is_removed());
        assert_eq!(resurrected.key, "k2");
        assert!(pool.has_available());
        // Reload from DB to confirm the suppression row was deleted —
        // otherwise the next start would re-tombstone it via the seed path.
        let pool2 = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("personal", "k2")]),
        )
        .unwrap();
        let live: Vec<_> = pool2
            .entries()
            .into_iter()
            .filter(|c| !c.is_removed())
            .collect();
        assert!(
            live.iter().any(|c| c.key == "k2"),
            "resurrected key survives reload, suppression cleared"
        );
    }

    #[test]
    fn remove_index_soft_deletes() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        let removed = pool.remove_index(1).unwrap();
        assert!(removed.is_removed());
        // At least one other entry remains pickable.
        let live = pool
            .entries()
            .into_iter()
            .filter(|c| !c.is_removed())
            .count();
        assert_eq!(live, 1);
        assert!(pool.has_available());
    }

    #[test]
    fn remove_index_out_of_range_returns_none() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        assert!(pool.remove_index(99).is_none());
        assert!(pool.remove_index(0).is_none());
    }

    #[test]
    fn resolve_target_by_label_index_and_id() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("primary", "k1"), ("backup", "k2")]),
        )
        .unwrap();
        // By label.
        let (_, c) = pool.resolve_target("backup").unwrap();
        assert_eq!(c.label, "backup");
        // By 1-based index.
        let (_, c) = pool.resolve_target("1").unwrap();
        assert_eq!(c.label, "primary");
        // By id.
        let id = pool.entries()[0].id.clone();
        let (_, c) = pool.resolve_target(&id).unwrap();
        assert_eq!(c.id, id);
        // Unknown.
        assert!(matches!(
            pool.resolve_target("nope"),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn resolve_target_ambiguous_label() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("same", "k1"), ("same", "k2")]),
        )
        .unwrap();
        assert!(matches!(
            pool.resolve_target("same"),
            Err(ResolveError::Ambiguous(_, _))
        ));
    }

    // ── A11. Persistence round-trip ──

    #[test]
    fn persistence_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let clock = TestClock::new(1_000_000);
        {
            let db = Database::open_at(&path).unwrap();
            let pool = CredentialPool::load_with_clock(
                &db,
                &path,
                Provider::OpenRouter,
                cfg_with(&[("a", "k1"), ("b", "k2")]),
                clock.clone(),
            )
            .unwrap();
            // Pick once so 'a' gets a use_count bump that must round-trip.
            let picked = pool.pick().unwrap();
            assert_eq!(picked.label, "a");
            let entries = pool.entries();
            let other_id = entries.iter().find(|c| c.label == "b").unwrap().id.clone();
            pool.report_failure(&other_id, FailoverReason::Auth, Some(401), None, None);
            pool.report_success(&picked.id);
        }
        // Re-open; state should persist.
        let db = Database::open_at(&path).unwrap();
        let pool = CredentialPool::load_with_clock(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
            clock,
        )
        .unwrap();
        let entries = pool.entries();
        let removed = entries.iter().find(|c| c.label == "b").unwrap();
        assert!(removed.is_removed());
        let first = entries.iter().find(|c| c.label == "a").unwrap();
        // use_count was bumped at pick() time.
        assert!(first.use_count >= 1);
    }

    #[test]
    fn remove_index_writes_suppression_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let var = "BORG_TEST_SUPPRESS_VAR";
        std::env::set_var(var, "envkey-secret");
        {
            let db = Database::open_at(&path).unwrap();
            let mut cfg = cfg_with(&[]);
            cfg.env_vars = vec![var.to_string()];
            let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
            assert_eq!(pool.entries().len(), 1);
            pool.remove_index(1).unwrap();
        }
        // Re-load with the env var still set — suppression should win.
        {
            let db = Database::open_at(&path).unwrap();
            let mut cfg = cfg_with(&[]);
            cfg.env_vars = vec![var.to_string()];
            let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
            let live = pool
                .entries()
                .into_iter()
                .filter(|c| !c.is_removed())
                .count();
            assert_eq!(live, 0, "suppression should prevent re-seed");
        }
        std::env::remove_var(var);
    }

    // ── Misc ──

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("abc").len(), 16);
    }

    #[test]
    fn strategy_parses_and_roundtrips() {
        use std::str::FromStr;
        for s in ["fill_first", "round_robin", "least_used", "random"] {
            let parsed = CredentialStrategy::from_str(s).unwrap();
            assert_eq!(parsed.to_string(), s);
        }
        assert!(CredentialStrategy::from_str("nope").is_err());
    }

    #[test]
    fn entries_includes_removed_tombstones() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        pool.remove_index(1);
        let entries = pool.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|c| c.is_removed()));
    }

    // ── Code-review regressions ──

    /// `peek` must not bump use_count or set `current_id`. The spec contract
    /// is "non-mutating preview"; tests pin it so a future refactor that
    /// inlines `peek -> pick` doesn't sneak past review.
    #[test]
    fn peek_is_non_mutating() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        for _ in 0..5 {
            let _ = pool.peek();
        }
        assert!(
            pool.entries().iter().all(|c| c.use_count == 0),
            "peek must not bump use_count"
        );
        assert!(pool.current().is_none(), "peek must not set current_id");
    }

    /// `acquire_lease(None)` must bump `use_count` — `LeastUsed` strategy
    /// relies on it. Regression for the bug where the lease path silently
    /// degraded LeastUsed to priority-only ordering.
    #[test]
    fn acquire_lease_bumps_use_count() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let lease = pool.acquire_lease(None).expect("lease");
        drop(lease);
        let count = pool.entries()[0].use_count;
        assert_eq!(count, 1, "acquire_lease must bump use_count");
    }

    /// A credential removed by 401/403 must stay removed across restart even
    /// when its env var is still set. Regression for the bug where
    /// `reconcile_or_new` dropped `removed_at_ms` on re-seed.
    #[test]
    fn auth_removal_survives_reload_with_env_still_set() {
        let (_db, path, _g) = open_test_db();
        let var = "BORG_TEST_AUTH_STICKY";
        std::env::set_var(var, "stickyk");
        let cred_id = {
            let db = Database::open_at(&path).unwrap();
            let mut cfg = cfg_with(&[]);
            cfg.env_vars = vec![var.to_string()];
            let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
            let live = pool.entries().into_iter().next().unwrap();
            pool.report_failure(
                &live.id,
                FailoverReason::Auth,
                Some(401),
                None,
                Some("revoked"),
            );
            assert!(pool.entries()[0].is_removed());
            live.id
        };
        // Re-load with the env var still present — removal must persist.
        {
            let db = Database::open_at(&path).unwrap();
            let mut cfg = cfg_with(&[]);
            cfg.env_vars = vec![var.to_string()];
            let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg).unwrap();
            let reloaded = pool
                .entries()
                .into_iter()
                .find(|c| c.id == cred_id)
                .expect("removed cred should still appear in entries");
            assert!(
                reloaded.is_removed(),
                "401-removed cred must not resurrect when env var still set"
            );
        }
        std::env::remove_var(var);
    }

    /// Plaintext `key` must never appear in `{:?}` output — accidental
    /// tracing of a `PooledCredential` (or a parent `PoolInner`) cannot
    /// leak the secret.
    #[test]
    fn debug_redacts_plaintext_key() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "supersecret-key-do-not-log")]),
        )
        .unwrap();
        let cred = &pool.entries()[0];
        let dbg_str = format!("{cred:?}");
        assert!(
            !dbg_str.contains("supersecret-key-do-not-log"),
            "Debug must not include plaintext key: {dbg_str}"
        );
        assert!(dbg_str.contains("redacted"), "Debug must mark redaction");
        // Also exercise the parent CredentialPool Debug (transitive through inner).
        let parent_dbg = format!("{pool:?}");
        assert!(
            !parent_dbg.contains("supersecret-key-do-not-log"),
            "CredentialPool Debug must not leak key transitively"
        );
    }

    // ── OAuth source seeding ──

    fn oauth_cfg(home: &std::path::Path) -> PoolLoadConfig {
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            max_concurrent_per_credential: 1,
            proactive_rate_limit_threshold_pct: 0,
            proactive_rate_limit_min_window_secs: 0,
            config_entries: vec![],
            env_vars: vec![],
            oauth_home: Some(home.to_path_buf()),
        }
    }

    #[test]
    fn load_seeds_claude_code_oauth_from_disk() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-abc","refreshToken":"sk-ant-ort-xyz","expiresAt":9999999999000,"subscriptionType":"pro","emailAddress":"alice@example.com"}}"#,
        )
        .unwrap();

        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        let entries = pool.entries();
        assert_eq!(entries.len(), 1, "one OAuth credential seeded");
        let c = &entries[0];
        assert_eq!(c.key, "sk-ant-oat-abc");
        assert_eq!(c.refresh_token.as_deref(), Some("sk-ant-ort-xyz"));
        assert_eq!(c.expires_at_ms, Some(9999999999000));
        assert!(matches!(c.auth_type, AuthType::OAuth));
        assert!(matches!(
            c.source,
            CredentialSource::OAuth { ref name } if name == "claude_code"
        ));
    }

    #[test]
    fn malformed_oauth_file_does_not_crash_load() {
        // Parser error must log + continue — never panic the daemon's startup
        // path because a single source file is corrupt.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            "not json {{ at all",
        )
        .unwrap();

        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        assert!(pool.entries().is_empty());
    }

    #[tokio::test]
    async fn acquire_lease_refreshing_renews_expired_oauth_token() {
        // Expired OAuth credential → refresh fires → access_token rotated,
        // refresh_token rotated, expires_at_ms moved forward.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"refreshed-access","refresh_token":"refreshed-r","expires_in":3600}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        // expiresAt in the past — forces refresh on next lease.
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"old-refresh","expiresAt":1}}"#,
        )
        .unwrap();

        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

        let lease = pool.acquire_lease_refreshing(None).await.unwrap();
        let cred = lease.credential().unwrap();
        assert_eq!(cred.key, "refreshed-access", "access token rotated");
        assert_eq!(
            cred.refresh_token.as_deref(),
            Some("refreshed-r"),
            "refresh token rotated"
        );
        assert!(
            cred.expires_at_ms.unwrap() > pool.clock.now_ms(),
            "expiry pushed into the future"
        );
    }

    #[tokio::test]
    async fn acquire_lease_refreshing_skips_when_token_still_valid() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0) // must NOT be called
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"still-good","refreshToken":"r","expiresAt":9999999999000}}"#,
        )
        .unwrap();
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

        let lease = pool.acquire_lease_refreshing(None).await.unwrap();
        assert_eq!(lease.credential().unwrap().key, "still-good");
        // wiremock's `expect(0)` validates on drop.
    }

    #[tokio::test]
    async fn refresh_failure_cools_credential_without_removing_it() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            // `.expect(1)` proves the cooldown was set BY the failed refresh,
            // not just by the credential happening to already be exhausted.
            // Without this, an inert fast-path (e.g. refresh skipped) would
            // silently pass — the "is not a tombstone" assertion alone is
            // also satisfied by a healthy credential.
            .expect(1)
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#,
        )
        .unwrap();
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));

        let lease = pool.acquire_lease_refreshing(None).await;
        assert!(
            lease.is_none(),
            "single expired OAuth credential with failed refresh should fall through to fallback, not reuse stale token"
        );
        let entries = pool.entries();
        assert!(
            !entries[0].is_removed(),
            "transient refresh failure is not a tombstone"
        );
        assert_eq!(
            entries[0].last_status,
            CredentialStatus::Exhausted,
            "refresh failure cools the credential"
        );
        assert!(
            entries[0].last_error_reset_at_ms.is_some(),
            "cooldown set so retries are spaced"
        );
        assert_eq!(
            entries[0].last_error_message.as_deref(),
            Some("oauth_refresh_failed"),
            "error message tags the cooldown as a refresh failure, distinguishing it from a true 429"
        );
        // wiremock's `.expect(1)` is verified on server drop.
        drop(server);
    }

    #[tokio::test]
    async fn refresh_failure_reacquires_a_healthy_sibling_credential() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"expired-access","refreshToken":"refresh-r","expiresAt":1}}"#,
        )
        .unwrap();
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        pool.set_refresh_endpoint_for_tests(Some(format!("{}/v1/oauth/token", server.uri())));
        let sibling = pool.add_entry(
            "healthy-api-key".into(),
            "manual-sibling".into(),
            CredentialSource::Manual,
        );

        let lease = pool
            .acquire_lease_refreshing(None)
            .await
            .expect("healthy sibling should be re-acquired");
        assert_eq!(
            lease.id(),
            sibling.id,
            "failed refresh must rotate to another available credential instead of keeping the stale one"
        );
    }

    #[test]
    fn may_recover_within_false_for_single_key_pool() {
        // Single key, 60s cooldown: there is nothing to rotate to, so
        // fallback should fire instead of waiting on the same exhausted key.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(60)),
            None,
        );
        assert!(
            !pool.may_recover_within(Duration::from_secs(300)),
            "single-key pool should not defer provider fallback"
        );
        assert!(
            !pool.may_recover_within(Duration::from_secs(30)),
            "single-key pool should never report recoverable-by-rotation"
        );
    }

    #[test]
    fn may_recover_within_true_for_multi_key_imminent_recovery() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(
            &db,
            &path,
            Provider::OpenRouter,
            cfg_with(&[("a", "k1"), ("b", "k2")]),
        )
        .unwrap();
        let entries = pool.entries();
        pool.report_failure(
            &entries[0].id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(60)),
            None,
        );
        assert!(
            pool.may_recover_within(Duration::from_secs(300)),
            "multi-key pool with an imminent recovery should defer fallback"
        );
        assert!(
            !pool.may_recover_within(Duration::from_secs(30)),
            "recovery outside the horizon should not defer fallback"
        );
    }

    #[test]
    fn may_recover_within_false_for_tombstoned_pool() {
        // Auth failure permanently tombstones — no recovery possible, so
        // fallback must engage.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(&cred.id, FailoverReason::Auth, Some(401), None, None);
        assert!(
            !pool.may_recover_within(Duration::from_secs(3600)),
            "tombstoned credential never recovers, no matter how long the window"
        );
    }

    #[test]
    fn may_recover_within_false_for_long_cooldown_beyond_horizon() {
        // 2-hour Retry-After exceeds the 5-min recovery window → fallback fires.
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, cfg_with(&[("a", "k1")]))
            .unwrap();
        let cred = pool.pick().unwrap();
        pool.report_failure(
            &cred.id,
            FailoverReason::RateLimit,
            None,
            Some(Duration::from_secs(7200)),
            None,
        );
        assert!(
            !pool.may_recover_within(Duration::from_secs(300)),
            "long cooldown beyond horizon → fallback should fire"
        );
    }

    #[test]
    fn needs_refresh_treats_missing_expiry_as_stale() {
        // Belt-and-suspenders: a legacy OAuth row with no expires_at_ms
        // forces one refresh roundtrip rather than gambling on a dead token.
        assert!(needs_refresh(None, 1_000_000));
        // Within the 60s skew window → refresh.
        assert!(needs_refresh(Some(1_000_000 + 30_000), 1_000_000));
        // Outside the skew window → no refresh.
        assert!(!needs_refresh(Some(1_000_000 + 120_000), 1_000_000));
    }

    #[test]
    fn oauth_seeded_credential_round_trips_through_persistence() {
        // Use-count and cooldown state from a prior run must survive a reload
        // even though the refresh_token is re-supplied by the discoverer (not
        // persisted to DB).
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok-1","refreshToken":"ref-1","expiresAt":1234567890000}}"#,
        )
        .unwrap();
        let (db, path, _g) = open_test_db();
        {
            let pool = CredentialPool::load(
                &db,
                &path,
                Provider::ClaudeCodeOAuth,
                oauth_cfg(home.path()),
            )
            .unwrap();
            let cred = pool.pick().unwrap();
            pool.report_failure(
                &cred.id,
                FailoverReason::RateLimit,
                None,
                Some(Duration::from_secs(60)),
                None,
            );
        }
        let pool2 = CredentialPool::load(
            &db,
            &path,
            Provider::ClaudeCodeOAuth,
            oauth_cfg(home.path()),
        )
        .unwrap();
        let entries = pool2.entries();
        assert_eq!(entries.len(), 1);
        let c = &entries[0];
        assert_eq!(
            c.refresh_token.as_deref(),
            Some("ref-1"),
            "refresh re-seeded from disk"
        );
        assert!(
            c.last_error_reset_at_ms.is_some(),
            "cooldown state survived reload"
        );
    }

    // ── Proactive rate-limit avoidance ──

    use crate::llm::rate_limit_headers::{Bucket, RateLimitSnapshot};

    fn snap_with_pct(now_ms: i64, remaining_pct: u8, reset_offset_ms: i64) -> RateLimitSnapshot {
        let limit = 1000u64;
        let remaining = (limit * remaining_pct as u64) / 100;
        RateLimitSnapshot {
            captured_at_ms: now_ms,
            requests: Some(Bucket {
                limit,
                remaining,
                reset_at_ms: now_ms + reset_offset_ms,
            }),
            tokens: None,
        }
    }

    fn proactive_cfg(threshold_pct: u8, min_window_secs: u64) -> PoolLoadConfig {
        let entries = vec![
            ConfigEntry {
                key: "key-a".into(),
                label: Some("A".into()),
                config_name: "llm.api_keys[0]".into(),
            },
            ConfigEntry {
                key: "key-b".into(),
                label: Some("B".into()),
                config_name: "llm.api_keys[1]".into(),
            },
        ];
        PoolLoadConfig {
            strategy: CredentialStrategy::FillFirst,
            cooldown_secs: 60,
            max_concurrent_per_credential: 1,
            proactive_rate_limit_threshold_pct: threshold_pct,
            proactive_rate_limit_min_window_secs: min_window_secs,
            config_entries: entries,
            env_vars: vec![],
            oauth_home: None,
        }
    }

    #[test]
    fn proactive_throttle_triggers_below_threshold() {
        let now = 1_000_000;
        let mut c = PooledCredential {
            id: "x".into(),
            label: "x".into(),
            priority: 0,
            source: CredentialSource::Manual,
            key: "k".into(),
            fingerprint: "fp".into(),
            use_count: 0,
            last_status: CredentialStatus::Ok,
            last_status_at_ms: Some(now),
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at_ms: None,
            removed_at_ms: None,
            auth_type: AuthType::ApiKey,
            refresh_token: None,
            expires_at_ms: None,
            base_url: None,
            extra_json: None,
            rate_limit: Some(snap_with_pct(now, 2, 60_000)),
        };
        // 2% remaining < 5% threshold, reset 60s out > 5s min window → throttled
        assert!(c.is_proactively_throttled(now, 5, 5));
        // Reset already passed → not throttled (snapshot is stale).
        assert!(!c.is_proactively_throttled(now + 70_000, 5, 5));
        // Within the min-window → waiting is cheaper, don't rotate.
        c.rate_limit = Some(snap_with_pct(now, 2, 3_000));
        assert!(!c.is_proactively_throttled(now, 5, 5));
        // Threshold disabled (0) → never throttle, even at 0% remaining.
        c.rate_limit = Some(snap_with_pct(now, 0, 60_000));
        assert!(!c.is_proactively_throttled(now, 0, 5));
        // No snapshot → not throttled.
        c.rate_limit = None;
        assert!(!c.is_proactively_throttled(now, 5, 5));
    }

    #[test]
    fn pick_skips_proactively_throttled_credential() {
        let (db, path, _g) = open_test_db();
        let pool = CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5))
            .expect("pool");

        let entries = pool.entries();
        let id_a = entries[0].id.clone();
        let id_b = entries[1].id.clone();

        // Mark A near-empty with a far-out reset; B stays healthy.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        pool.report_rate_limit_headers(&id_a, snap_with_pct(now, 1, 120_000));

        // FillFirst would normally pick A (lower priority). With the proactive
        // filter it should skip to B.
        let picked = pool.pick().expect("pick");
        assert_eq!(
            picked.id, id_b,
            "pool should skip near-empty credential A in favor of healthy B"
        );

        // Now mark B near-empty too. Every credential is throttled, but the
        // fallback pass must still pick *something* — otherwise the caller
        // would synthesize an exhaustion error for a key that's actually live.
        pool.report_rate_limit_headers(&id_b, snap_with_pct(now, 1, 120_000));
        let picked = pool.pick().expect("pick under all-throttled");
        assert!(
            picked.id == id_a || picked.id == id_b,
            "fallback pick must still return a live credential"
        );
    }

    #[test]
    fn report_rate_limit_headers_overwrites_prior_snapshot() {
        // Regression guard: the latest provider observation always wins. If we
        // ever changed `report_rate_limit_headers` to merge / max / preserve
        // an older snapshot, a recovered bucket would never re-enable a
        // credential until something else cleared the field.
        let (db, path, _g) = open_test_db();
        let pool =
            CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5)).unwrap();
        let id = pool.entries()[0].id.clone();
        let now = 2_000_000;
        pool.report_rate_limit_headers(&id, snap_with_pct(now, 1, 60_000));
        pool.report_rate_limit_headers(&id, snap_with_pct(now, 90, 60_000));
        let snap = pool.entries()[0].rate_limit.clone().expect("snapshot");
        assert_eq!(
            snap.requests.unwrap().remaining,
            900,
            "latest snapshot must replace the prior one, not merge"
        );
    }

    #[test]
    fn report_rate_limit_headers_on_removed_is_noop() {
        let (db, path, _g) = open_test_db();
        let pool =
            CredentialPool::load(&db, &path, Provider::OpenRouter, proactive_cfg(5, 5)).unwrap();
        let id = pool.entries()[0].id.clone();
        // Tombstone it.
        pool.report_failure(&id, FailoverReason::Auth, Some(401), None, None);
        let now = 3_000_000;
        pool.report_rate_limit_headers(&id, snap_with_pct(now, 1, 60_000));
        // Snapshot must not have been attached to a tombstoned credential —
        // otherwise `borg auth list` shows phantom bucket state on a key that
        // can never be used again.
        assert!(pool.entries()[0].rate_limit.is_none());
    }
}
