//! Save a raw API key to the OS keychain and wire up `llm.api_key` as a
//! `SecretRef::Exec` that reads it back. Used by the `/settings` popup and
//! `/model` popup to let users update provider credentials without re-running
//! onboarding.
//!
//! Mirrors the post-onboarding keychain path in `onboarding.rs` but packaged
//! as a standalone helper so the TUI callers can invoke it in isolation.

use std::str::FromStr;

use anyhow::{Context, Result};

use borg_core::config::Config;
use borg_core::db::Database;
use borg_core::provider::Provider;
use borg_core::secrets_resolve::SecretRef;

/// Outcome of attempting to save an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeySaveOutcome {
    /// Key was written to the OS keychain; `llm.api_key` now points at it.
    StoredInKeychain,
    /// No keychain available on this platform. Caller should show the user
    /// the fallback instructions (set `$ENV_VAR` in the shell).
    KeychainUnavailable,
    /// Input was empty after trimming; nothing written.
    EmptyInput,
}

/// Keychain service name convention for a provider.
pub fn keychain_service_name(provider_id: &str) -> String {
    format!("borg-{provider_id}")
}

/// Build the `SecretRef::Exec` JSON that reads the stored key back out.
/// Matches the shape used by onboarding so keys persist uniformly.
fn build_secret_ref_json(provider_id: &str) -> String {
    let service = keychain_service_name(provider_id);
    if cfg!(target_os = "macos") {
        format!(
            r#"{{"source":"exec","command":"security","args":["find-generic-password","-s","{service}","-a","borg","-w"]}}"#,
        )
    } else if cfg!(target_os = "linux") {
        format!(
            r#"{{"source":"exec","command":"secret-tool","args":["lookup","service","borg","provider","{provider_id}"]}}"#,
        )
    } else if cfg!(target_os = "windows") {
        // Windows doesn't expose a stdout lookup for Credential Manager via
        // `cmdkey`, so we fall back to letting the caller surface the
        // keychain-unavailable hint. This branch is defensive — `save_api_key`
        // gates on `keychain::available()` before calling this.
        format!(r#"{{"source":"exec","command":"cmdkey","args":["/list:{service}/borg"]}}"#,)
    } else {
        String::new()
    }
}

/// Save `raw_key` to the OS keychain under a stable service name derived from
/// `provider_id`, then persist a `SecretRef::Exec` pointing at it to both the
/// database and the live `cfg` so the new key takes effect immediately.
///
/// Returns `EmptyInput` for empty/whitespace-only input without touching any
/// state. Returns `KeychainUnavailable` when there's no keychain backend on
/// the host — the caller should surface the env-var fallback hint to the user
/// rather than persisting the secret anywhere.
pub fn save_api_key(
    db: &Database,
    cfg: &mut Config,
    provider_id: &str,
    raw_key: &str,
) -> Result<ApiKeySaveOutcome> {
    let clean_key = raw_key.trim().replace(['\n', '\r'], "");
    if clean_key.is_empty() {
        return Ok(ApiKeySaveOutcome::EmptyInput);
    }

    if !borg_plugins::keychain::available() {
        return Ok(ApiKeySaveOutcome::KeychainUnavailable);
    }

    let service = keychain_service_name(provider_id);
    borg_plugins::keychain::store(&service, "borg", &clean_key)
        .with_context(|| format!("Failed to write API key to keychain (service={service})"))?;

    let secret_json = build_secret_ref_json(provider_id);
    let secret_ref: SecretRef =
        serde_json::from_str(&secret_json).context("Failed to parse generated SecretRef JSON")?;

    db.set_setting("llm.api_key", &secret_json)
        .context("Failed to persist llm.api_key in database")?;

    cfg.llm.api_key = Some(secret_ref);

    Ok(ApiKeySaveOutcome::StoredInKeychain)
}

/// Env-var hint shown when keychain storage is unavailable. Returned as a
/// separate helper so both the `/settings` and `/model` popups display
/// identical wording.
pub fn env_var_hint(provider_id: &str) -> String {
    let env_var = Provider::from_str(provider_id)
        .map(|p| p.default_env_var())
        .unwrap_or("OPENROUTER_API_KEY");
    format!(
        "No keychain available. Set {env_var} in your shell and restart borg, or re-run onboarding."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty_outcome() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).expect("db");
        let mut cfg = Config::default();
        let outcome = save_api_key(&db, &mut cfg, "openrouter", "   ").unwrap();
        assert_eq!(outcome, ApiKeySaveOutcome::EmptyInput);
        assert!(cfg.llm.api_key.is_none());
    }

    #[test]
    fn keychain_service_name_format() {
        assert_eq!(keychain_service_name("openrouter"), "borg-openrouter");
        assert_eq!(keychain_service_name("anthropic"), "borg-anthropic");
    }

    #[test]
    fn env_var_hint_mentions_env_var() {
        let hint = env_var_hint("openrouter");
        assert!(hint.contains("OPENROUTER_API_KEY"), "hint was: {hint}");
        let hint = env_var_hint("anthropic");
        assert!(hint.contains("ANTHROPIC_API_KEY"), "hint was: {hint}");
    }

    #[test]
    fn secret_ref_json_is_valid() {
        let json = build_secret_ref_json("openrouter");
        let parsed: Result<SecretRef, _> = serde_json::from_str(&json);
        assert!(parsed.is_ok(), "invalid JSON: {json}");
    }

    // Keychain round-trip is exercised in `crates/plugins/src/keychain.rs`
    // under `#[cfg(target_os = "macos")]`. We avoid duplicating that here so
    // this crate's test suite stays platform-agnostic.
}
