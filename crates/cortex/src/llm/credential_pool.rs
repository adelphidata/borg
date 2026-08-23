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
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::db::Database;
use crate::llm::llm_error::FailoverReason;
use crate::llm::provider::Provider;
use crate::llm::rate_limit_headers::RateLimitSnapshot;

mod builders;
mod helpers;
pub use builders::{
    pool_load_config_for_provider, pool_load_config_for_provider_with_proactive,
    try_build_from_keys,
};
pub use helpers::{fingerprint, is_safe_base_url};
use helpers::{
    needs_refresh, random_id, truncate_error_msg, COOLDOWN_MAX_SECS,
    RATE_LIMIT_ESCALATION_MAX_SHIFT,
};

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
    /// True once a 401 has already spent this credential's one forced-refresh
    /// grace. An OAuth credential's first `Auth` failure force-expires the
    /// access token (the next lease acquisition refreshes it) instead of
    /// tombstoning — a stale-but-unexpired token is fixable. A second `Auth`
    /// failure while this flag is set tombstones as usual. Cleared on
    /// `report_success`. In-memory only.
    pub auth_refresh_grace_used: bool,
    /// Consecutive `RateLimit` failures with no intervening success.
    ///
    /// Drives geometric cooldown escalation so a key stuck behind a hard cap
    /// stops being re-probed at the base interval indefinitely. In-memory
    /// only: a restart is a fresh observation window, which is the right
    /// default for a signal this coarse. Cleared by `report_success` and by
    /// any non-`RateLimit` failure.
    pub consecutive_rate_limits: u32,
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
    /// Bench applied on a billing failure with no provider `Retry-After`.
    /// Separate from `cooldown` because billing exhaustion does not clear on
    /// its own the way a throttle does.
    billing_cooldown: Duration,
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

/// Whether a benched credential is worth one last-gasp attempt.
///
/// `Billing` (a spent account) and `AuthPermanent` (a revoked or invalid key)
/// do not clear with time — only a human can fix them. Every other bench,
/// including `RateLimit`, is a window that expires, so a salvage attempt is at
/// worst one wasted request and at best keeps a single-key install alive.
///
/// Reads the persisted `last_error_reason` string rather than a typed field
/// because that is what survives a restart; the strings come from
/// [`FailoverReason`]'s `Display`, so they are compared against it directly
/// instead of being hardcoded.
fn is_salvageable(c: &PooledCredential, now_ms: i64) -> bool {
    // An OAuth credential whose access token has expired needs the serialized
    // refresh that `acquire_lease_refreshing` performs. Salvage hands the
    // credential back directly, bypassing that, so a stale token would go on
    // the wire, 401, and spend the one-shot `auth_refresh_grace` — turning a
    // recoverable session into a tombstoned credential.
    if matches!(c.auth_type, AuthType::OAuth) {
        if let Some(expires_at) = c.expires_at_ms {
            if expires_at <= now_ms {
                return false;
            }
        }
    }
    let Some(reason) = c.last_error_reason.as_deref() else {
        return true;
    };
    reason != FailoverReason::Billing.to_string()
        && reason != FailoverReason::AuthPermanent.to_string()
}

/// Shared tail of the two cooldown arms: bench the credential until
/// `now + dur`, extend-never-shorten, and record the diagnosis.
///
/// Factored out so `RateLimit` and `Billing` differ *only* in how they derive
/// `dur` — the state they write must not drift apart.
fn apply_cooldown(
    c: &mut PooledCredential,
    now: i64,
    dur: u64,
    reason: FailoverReason,
    status_code: Option<u16>,
    truncated: Option<String>,
) {
    let new_until = now + (dur as i64) * 1000;
    // Extend, never shorten — a second failure carrying a longer hint should
    // win, and a short hint must not release a credential benched for longer.
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
        consecutive_rate_limits = c.consecutive_rate_limits,
        "credential cooled"
    );
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
    /// Cooldown applied on a billing failure (402 / hard 403 quota) when the
    /// provider gives no `Retry-After`.
    pub billing_cooldown_secs: u64,
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
        let suppressed = load_suppressions(conn, provider_str).await?;

        // 2. Load persisted rows (state-bearing tombstones included).
        let persisted = load_persisted_rows(conn, provider_str).await?;

        // 3. Seed: config entries, then env vars. Suppressed sources skipped.
        let mut seed = SeedState::default();
        seed_config_entries(&cfg, &suppressed, &persisted, clock.as_ref(), &mut seed);
        seed_env_vars(&cfg, &suppressed, &persisted, clock.as_ref(), &mut seed);

        // 3b. Seed: OAuth and custom-endpoint credentials discovered on disk.
        seed_discovered_credentials(
            provider,
            &cfg,
            &suppressed,
            &persisted,
            clock.as_ref(),
            &mut seed,
        );

        let SeedState {
            mut live, seen_fp, ..
        } = seed;

        // 4. Carry forward removed-tombstone rows for `borg auth list` history.
        carry_forward_tombstones(&persisted, &seen_fp, &mut live);

        // 5. Prune persisted rows whose source disappeared from env/config and
        //    that aren't tombstoned (so `borg auth list` still shows manual
        //    removals); the live ones we already reconciled above.
        prune_vanished_rows(conn, provider_str, &persisted, &live).await?;

        // 6. Normalize priorities (live first, tombstones at the end).
        normalize_priorities(&mut live);

        // 7. Persist (write-through reflects normalized priorities).
        for c in &live {
            upsert_row(conn, provider_str, c).await?;
        }

        let inner = PoolInner {
            creds: live,
            strategy: cfg.strategy,
            cooldown: Duration::from_secs(cfg.cooldown_secs.max(1)),
            billing_cooldown: Duration::from_secs(cfg.billing_cooldown_secs.max(1)),
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

    /// Number of retained per-credential OAuth refresh locks.
    ///
    /// Exposed so the bounded-growth property is testable: the map is swept on
    /// each use and should only retain entries with a refresh in flight.
    pub fn refresh_lock_count(&self) -> usize {
        self.refresh_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
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

    /// Bench length applied to a billing failure with no provider hint.
    pub fn billing_cooldown_secs(&self) -> u64 {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.billing_cooldown.as_secs()
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
    ///
    /// Entries benched for a reason that cannot clear on its own — `Billing`
    /// (a spent account) and `AuthPermanent` (a dead key) — are skipped. Both
    /// need human action, so re-attempting them buys nothing and re-creates the
    /// every-60s hammering that the long billing bench exists to prevent. An
    /// OAuth credential with an expired access token is skipped too, since this
    /// path bypasses the refresh. Returns `None` when no entry qualifies.
    pub fn pick_force(&self) -> Option<PooledCredential> {
        let now = self.clock.now_ms();
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pick_id = g
            .creds
            .iter()
            .filter(|c| !c.is_removed() && is_salvageable(c, now))
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
        // A completed request re-arms the one-shot 401 refresh grace: the
        // next auth incident is a fresh one, not a continuation.
        c.auth_refresh_grace_used = false;
        // A completed request also ends the consecutive-throttle streak, so
        // the next 429 starts from the short base cooldown again.
        c.consecutive_rate_limits = 0;
        g.emitted_exhausted_warn = false;
        let cred = g.creds.iter().find(|c| c.id == credential_id).cloned();
        drop(g);
        if let Some(c) = cred {
            self.persist_credential(&c);
        }
    }

    /// Record a failed request. Maps `reason` to the appropriate state
    /// transition: Auth/AuthPermanent → soft-delete; RateLimit → escalating
    /// cooldown; Billing → the long `billing_cooldown` bench; everything else
    /// is a no-op on the pool (transport / payload concerns).
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
        let billing_cooldown = g.billing_cooldown;
        // Computed before the mutable borrow below. Escalation exists to stop a
        // capped key from stealing rotation slots from its siblings; with no
        // siblings there is nothing to protect and the escalation only benches
        // the one credential the agent has.
        let is_sole_credential = g.creds.iter().filter(|c| !c.is_removed()).count() <= 1;
        let Some(c) = g.creds.iter_mut().find(|c| c.id == credential_id) else {
            return;
        };
        if c.is_removed() {
            return;
        }
        let truncated = error_message.map(truncate_error_msg);

        match reason {
            // First 401 on an OAuth credential that can refresh: the access
            // token may merely be stale (revoked-and-rotated by another
            // device, clock skew) while the refresh token is still good.
            // Force-expire the access token so the next
            // `acquire_lease_refreshing` runs the serialized refresh, and
            // spend the one-shot grace flag. Bounded: a second Auth failure
            // with the flag set falls through to the tombstone arm below,
            // and 403 (`AuthPermanent`) never gets the grace.
            FailoverReason::Auth
                if matches!(c.auth_type, AuthType::OAuth)
                    && c.refresh_token.is_some()
                    && !c.auth_refresh_grace_used =>
            {
                c.auth_refresh_grace_used = true;
                c.expires_at_ms = Some(now - 1);
                c.last_status = CredentialStatus::Ok;
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
                    status_code = ?status_code,
                    "auth failure on refreshable OAuth credential — forcing one token refresh before removal"
                );
            }
            FailoverReason::Auth | FailoverReason::AuthPermanent => {
                c.consecutive_rate_limits = 0;
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
            FailoverReason::RateLimit => {
                c.consecutive_rate_limits = c.consecutive_rate_limits.saturating_add(1);
                // A provider hint is ground truth about its own bucket and
                // always wins. Absent one, escalate geometrically: a key
                // sitting behind a hard per-hour cap should not be re-probed
                // every 60s all night, but a merely bursty key must still
                // recover fast, so the first hit keeps the short cooldown.
                //
                // Escalation is a *rotation* policy: it stops one capped key
                // from being re-probed ahead of healthy siblings. A pool of one
                // has no siblings, so escalating there buys nothing and costs
                // everything — the agent is simply down for up to
                // COOLDOWN_MAX_SECS on what may be a one-second burst. Hold
                // such a pool at the base cooldown and let it re-probe.
                let dur = match retry_after {
                    Some(d) => d.as_secs().clamp(1, COOLDOWN_MAX_SECS),
                    None if is_sole_credential => cooldown.as_secs().clamp(1, COOLDOWN_MAX_SECS),
                    None => {
                        let shift = c
                            .consecutive_rate_limits
                            .saturating_sub(1)
                            .min(RATE_LIMIT_ESCALATION_MAX_SHIFT);
                        cooldown
                            .as_secs()
                            .saturating_mul(1u64 << shift)
                            .clamp(1, COOLDOWN_MAX_SECS)
                    }
                };
                apply_cooldown(c, now, dur, reason, status_code, truncated);
            }
            FailoverReason::Billing => {
                // Billing exhaustion does not clear on its own the way a
                // throttle does. Sharing the throttle cooldown meant a spent
                // account was retried every 60s forever, burning the rest of
                // the pool's rotation budget on a key that cannot recover
                // without human action.
                c.consecutive_rate_limits = 0;
                let dur = retry_after
                    .map(|d| d.as_secs().clamp(1, COOLDOWN_MAX_SECS))
                    .unwrap_or_else(|| billing_cooldown.as_secs().clamp(1, COOLDOWN_MAX_SECS));
                apply_cooldown(c, now, dur, reason, status_code, truncated);
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
            auth_refresh_grace_used: false,
            consecutive_rate_limits: 0,
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
        // In-flight load comes first — piling a second concurrent request onto
        // a credential while a sibling sits idle wastes the pool regardless of
        // strategy. Among equally-loaded candidates, `llm.credential_strategy`
        // decides.
        //
        // The strategy used to be consulted only by `pick()`, which has no
        // production call site, so every strategy behaved identically
        // (fill-first) and the setting was inert.
        let leases_of = |g: &PoolInner, i: usize| -> u32 {
            g.active_leases.get(&g.creds[i].id).copied().unwrap_or(0)
        };
        let min_leases = candidates.iter().map(|&i| leases_of(&g, i)).min()?;
        let tied: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| leases_of(&g, i) == min_leases)
            .collect();

        let pool_idx = match g.strategy {
            CredentialStrategy::FillFirst => *tied.iter().min_by_key(|&&i| g.creds[i].priority)?,
            CredentialStrategy::RoundRobin => {
                // First candidate ranked after the current one by priority,
                // wrapping to the lowest priority at the end of the cycle.
                let current_priority = g
                    .current_id
                    .as_ref()
                    .and_then(|id| g.creds.iter().find(|c| &c.id == id))
                    .map(|c| c.priority);
                match current_priority {
                    Some(cur) => tied
                        .iter()
                        .filter(|&&i| g.creds[i].priority > cur)
                        .min_by_key(|&&i| g.creds[i].priority)
                        .or_else(|| tied.iter().min_by_key(|&&i| g.creds[i].priority))
                        .copied()?,
                    None => *tied.iter().min_by_key(|&&i| g.creds[i].priority)?,
                }
            }
            CredentialStrategy::LeastUsed => *tied.iter().min_by(|&&a, &&b| {
                g.creds[a]
                    .use_count
                    .cmp(&g.creds[b].use_count)
                    .then(g.creds[a].priority.cmp(&g.creds[b].priority))
            })?,
            CredentialStrategy::Random => {
                let mut rng = rand::rng();
                *tied.choose(&mut rng)?
            }
        };

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
            // Opportunistic sweep: an entry whose only strong reference is the
            // map itself has no in-flight refresh holding it, so dropping it
            // cannot break mutual exclusion — a concurrent caller has already
            // cloned the `Arc` (count > 1) before it can await the lock. Left
            // unswept the map grew one entry per credential ever leased and
            // never shrank, including for credentials later tombstoned by a
            // 401/403.
            locks.retain(|_, l| Arc::strong_count(l) > 1);
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

/// Accumulator threaded through the seeding phases of `load_with_clock`:
/// credentials seeded so far, fingerprints already claimed (first source
/// wins), and the next priority slot.
#[derive(Default)]
struct SeedState {
    live: Vec<PooledCredential>,
    seen_fp: HashSet<String>,
    next_priority: u32,
}

/// Load the operator suppression set for `provider` (step 1 of pool load).
async fn load_suppressions(
    conn: &libsql::Connection,
    provider_str: &str,
) -> Result<HashSet<(String, String)>> {
    let mut suppressed: HashSet<(String, String)> = HashSet::new();
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
    Ok(suppressed)
}

/// Load persisted pool rows keyed by fingerprint, state-bearing tombstones
/// included (step 2 of pool load).
async fn load_persisted_rows(
    conn: &libsql::Connection,
    provider_str: &str,
) -> Result<HashMap<String, PersistedRow>> {
    let mut persisted: HashMap<String, PersistedRow> = HashMap::new();
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
        let row = parse_persisted_row(&r)?;
        persisted.insert(row.fingerprint.clone(), row);
    }
    Ok(persisted)
}

/// Decode one `credential_pool` row in the column order of the SELECT in
/// [`load_persisted_rows`].
fn parse_persisted_row(r: &libsql::Row) -> Result<PersistedRow> {
    let auth_type_str: String = r.get::<String>(13)?;
    let auth_type = match auth_type_str.as_str() {
        "oauth" => AuthType::OAuth,
        _ => AuthType::ApiKey,
    };
    Ok(PersistedRow {
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
    })
}

/// Seed config-block credentials (step 3 of pool load). Empty keys,
/// suppressed sources, and duplicate fingerprints are skipped.
fn seed_config_entries(
    cfg: &PoolLoadConfig,
    suppressed: &HashSet<(String, String)>,
    persisted: &HashMap<String, PersistedRow>,
    clock: &dyn Clock,
    seed: &mut SeedState,
) {
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
        if !seed.seen_fp.insert(fingerprint.clone()) {
            continue;
        }
        let label = entry
            .label
            .clone()
            .unwrap_or_else(|| entry.config_name.clone());
        let cred = reconcile_or_new(
            persisted,
            fingerprint,
            entry.key.clone(),
            label,
            source,
            seed.next_priority,
            clock.now_ms(),
        );
        seed.next_priority += 1;
        seed.live.push(cred);
    }
}

/// Seed env-var credentials after config entries (step 3 of pool load).
/// Unset/empty vars, suppressed sources, and duplicate fingerprints are
/// skipped.
fn seed_env_vars(
    cfg: &PoolLoadConfig,
    suppressed: &HashSet<(String, String)>,
    persisted: &HashMap<String, PersistedRow>,
    clock: &dyn Clock,
    seed: &mut SeedState,
) {
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
        if !seed.seen_fp.insert(fingerprint.clone()) {
            continue;
        }
        let cred = reconcile_or_new(
            persisted,
            fingerprint,
            val,
            var.clone(),
            source,
            seed.next_priority,
            clock.now_ms(),
        );
        seed.next_priority += 1;
        seed.live.push(cred);
    }
}

/// Seed OAuth and custom-endpoint credentials discovered on disk (step 3b of
/// pool load). Discoverers are matched per-provider; sources that find
/// nothing return Ok(vec![]). Parser errors log + continue — a malformed
/// credentials file must never crash pool load.
fn seed_discovered_credentials(
    provider: Provider,
    cfg: &PoolLoadConfig,
    suppressed: &HashSet<(String, String)>,
    persisted: &HashMap<String, PersistedRow>,
    clock: &dyn Clock,
    seed: &mut SeedState,
) {
    let oauth_home = cfg.oauth_home.clone().or_else(dirs::home_dir);
    let Some(home) = oauth_home.as_deref() else {
        return;
    };
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
            if !seed.seen_fp.insert(fp.clone()) {
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
                persisted,
                fp,
                disc,
                source,
                auth_type,
                seed.next_priority,
                clock.now_ms(),
            );
            seed.next_priority += 1;
            seed.live.push(cred);
        }
    }
}

/// Carry forward removed-tombstone rows for `borg auth list` history (step 4
/// of pool load). Rows whose `source_json` no longer parses are dropped.
fn carry_forward_tombstones(
    persisted: &HashMap<String, PersistedRow>,
    seen_fp: &HashSet<String>,
    live: &mut Vec<PooledCredential>,
) {
    for (fp, row) in persisted {
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
                    auth_refresh_grace_used: false,
                    consecutive_rate_limits: 0,
                });
            }
        }
    }
}

/// Delete persisted rows whose source disappeared and that aren't carried
/// forward as tombstones (step 5 of pool load).
async fn prune_vanished_rows(
    conn: &libsql::Connection,
    provider_str: &str,
    persisted: &HashMap<String, PersistedRow>,
    live: &[PooledCredential],
) -> Result<()> {
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
    Ok(())
}

/// Re-number priorities to a dense 0..n ordering — live entries first,
/// tombstones at the end (step 6 of pool load).
fn normalize_priorities(live: &mut [PooledCredential]) {
    live.sort_by(|a, b| {
        a.removed_at_ms
            .is_some()
            .cmp(&b.removed_at_ms.is_some())
            .then(a.priority.cmp(&b.priority))
    });
    for (i, c) in live.iter_mut().enumerate() {
        c.priority = i as u32;
    }
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
            auth_refresh_grace_used: false,
            consecutive_rate_limits: 0,
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
            auth_refresh_grace_used: false,
            consecutive_rate_limits: 0,
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
            auth_refresh_grace_used: false,
            consecutive_rate_limits: 0,
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
            auth_refresh_grace_used: false,
            consecutive_rate_limits: 0,
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

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests;
