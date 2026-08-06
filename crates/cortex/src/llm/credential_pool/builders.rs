//! Public builders for `CredentialPool` setup.
//!
//! Three entry points that wrap `CredentialPool::load`:
//!   - [`pool_load_config_for_provider`] / [`pool_load_config_for_provider_with_proactive`]:
//!     compose a `PoolLoadConfig` from the resolved provider plus already-
//!     plaintext config entries. Callers (`LlmClient::new`, settings UI)
//!     resolve the SecretRef chain before invoking these.
//!   - [`try_build_from_keys`]: end-to-end best-effort builder used by
//!     `LlmClient::new` and the auth subcommands. Returns `None` and logs
//!     at WARN on every failure path so prod regressions are observable.
//!
//! Lives in its own module because the bodies are mostly clerical — moving
//! them out of `credential_pool.rs` keeps the giant `impl CredentialPool`
//! block uncluttered.

use std::sync::Arc;

use tracing::warn;

use crate::db::Database;
use crate::llm::Provider;

use super::{ConfigEntry, CredentialPool, CredentialStrategy, PoolLoadConfig};

/// Fallback bench for a billing failure when the caller does not thread
/// `llm.billing_cooldown_secs` through. Mirrors the config default.
pub const DEFAULT_BILLING_COOLDOWN_SECS: u64 = 3600;

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
        // Callers that care override this; the default keeps a spent key off
        // the rotation for an hour rather than re-probing it every minute.
        billing_cooldown_secs: DEFAULT_BILLING_COOLDOWN_SECS,
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
/// Takes the whole `LlmConfig` rather than the six tuning knobs individually:
/// every caller was unpacking the same fields from it, and threading a seventh
/// knob through by position is how the two cooldowns ended up conflated in the
/// first place.
pub async fn try_build_from_keys(
    provider: Provider,
    plaintext_keys: Vec<String>,
    llm: &crate::config::LlmConfig,
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
    let mut cfg = pool_load_config_for_provider_with_proactive(
        provider,
        llm.credential_strategy,
        llm.credential_cooldown_secs,
        llm.max_concurrent_per_credential,
        llm.proactive_rate_limit_threshold_pct,
        llm.proactive_rate_limit_min_window_secs,
        config_entries,
    );
    cfg.billing_cooldown_secs = llm.billing_cooldown_secs;
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
