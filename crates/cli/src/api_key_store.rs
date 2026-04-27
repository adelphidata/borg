//! Save a raw API key to the OS keychain (or as a literal) and persist the
//! routing metadata to the daemon so `llm.api_key` resolves it on the next
//! config reload.
//!
//! All DB access funnels through admin RPCs (`SetApiKey`/`GetApiKey`/etc.).
//! The keychain `keyring` calls stay client-side — the daemon never touches
//! the OS keychain.
//!
//! Public function shapes are unchanged from the pre-RPC version so
//! settings-popup / model-popup callers don't need updates. The first
//! parameter is now ignored (kept generic so existing callers passing a
//! `&Database` continue to compile through this transitional stage).

use std::str::FromStr;

use anyhow::{Context, Result};

use borg_core::config::Config;
use borg_core::provider::Provider;
use borg_core::secrets_resolve::SecretRef;
use borg_proto::admin::{api_key_value::Body as ApiKeyBody, ApiKeyValue, KeychainRef};

use crate::daemon_client;

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

/// Run `fut` to completion regardless of whether we're already inside a
/// tokio runtime. `block_in_place` releases the worker thread so the inner
/// `block_on` can drive other tasks (e.g. the gRPC connector). When called
/// from a non-runtime thread we fall back to spinning a small current-thread
/// runtime — necessary for sync test contexts and the rare CLI subcommand
/// that calls this without `#[tokio::main]`.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(fut)),
        Err(_) => {
            // No ambient runtime — build a single-threaded one for this call.
            // If runtime init fails the host is in such a degraded state that
            // panicking surfaces the real fault faster than swallowing it.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => panic!("api_key_store: failed to spin a tokio runtime: {e}"),
            };
            rt.block_on(fut)
        }
    }
}

/// Save `raw_key` for `provider_id`. The keychain write happens locally;
/// the daemon stores routing metadata so subsequent `Config::load_from_db`
/// rebuilds `llm.api_key` correctly.
///
/// Returns `EmptyInput` for empty/whitespace-only input without touching any
/// state. Returns `KeychainUnavailable` when there's no keychain backend on
/// the host — the caller should surface the env-var fallback hint to the user
/// rather than persisting the secret anywhere.
///
/// The first parameter is generic and ignored; it exists so callers that
/// still hold a `&Database` reference compile through this transitional
/// stage. Once those callers move off the DB handle the parameter will be
/// dropped.
pub fn save_api_key<D>(
    _db: &D,
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

    // Persist routing metadata via the daemon. The keychain entry is the
    // source of truth for the secret; `credentials.{provider}` records WHERE
    // to read it so other clients (e.g. the daemon itself) can resolve it.
    // We also write `llm.api_key` so the existing resolver path picks the
    // new key up on the next config reload without needing a per-provider
    // lookup change.
    let provider = provider_id.to_string();
    let api_key_value = ApiKeyValue {
        provider,
        body: Some(ApiKeyBody::Keychain(KeychainRef {
            service,
            account: "borg".into(),
        })),
    };
    let llm_api_key_json = secret_json;
    block_on(async move {
        let mut client = daemon_client::connect(None)
            .await
            .context("connect to borgd for api-key persistence")?;
        client
            .set_api_key(api_key_value)
            .await
            .context("Admin.SetApiKey failed")?;
        client
            .set_setting("llm.api_key", &llm_api_key_json)
            .await
            .context("Admin.SetSetting(llm.api_key) failed")
    })?;

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

    // The empty-input fast path doesn't need a daemon; it returns before any
    // RPC. Exercising it here guards against a future refactor that moves
    // the trim-check after the network call (which would force callers to
    // run a daemon just to reject empty input).
    #[test]
    fn empty_input_returns_empty_outcome_without_rpc() {
        let mut cfg = Config::default();
        // Pass `&()` as the unused db handle — the generic parameter accepts
        // any type, which is the whole point of the transitional shim.
        let outcome = save_api_key(&(), &mut cfg, "openrouter", "   ").unwrap();
        assert_eq!(outcome, ApiKeySaveOutcome::EmptyInput);
        assert!(cfg.llm.api_key.is_none());
    }

    // Keychain round-trip is exercised in `crates/plugins/src/keychain.rs`
    // under `#[cfg(target_os = "macos")]`. We avoid duplicating that here so
    // this crate's test suite stays platform-agnostic. The daemon RPC paths
    // (set/get/unset/list) are covered end-to-end in
    // `crates/borgd/tests/admin_rpc_smoke.rs`.
}
