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

use cortex::config::Config;
use cortex::llm::provider::Provider;
use cortex::security::secrets_resolve::provider_api_key_secret_ref;
use protocols::admin::{api_key_value::Body as ApiKeyBody, ApiKeyValue, KeychainRef};

use crate::daemon_client;

#[cfg(test)]
use cortex::security::secrets_resolve::SecretRef;

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

    if !plugins::keychain::available() {
        return Ok(ApiKeySaveOutcome::KeychainUnavailable);
    }

    let service = keychain_service_name(provider_id);
    plugins::keychain::store(&service, "borg", &clean_key)
        .with_context(|| format!("Failed to write API key to keychain (service={service})"))?;

    let secret_ref = provider_api_key_secret_ref(provider_id);
    let secret_json =
        serde_json::to_string(&secret_ref).context("Failed to serialize SecretRef JSON")?;

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
    fn provider_api_key_secret_ref_is_canonical_keychain_ref() {
        let secret_ref = provider_api_key_secret_ref("openrouter");
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                assert_eq!(service, "borg-openrouter");
                assert_eq!(account, "borg");
            }
            other => panic!("expected Keychain variant, got {other:?}"),
        }
    }

    #[test]
    fn provider_api_key_secret_ref_json_round_trip() {
        let secret_ref = provider_api_key_secret_ref("openrouter");
        let json = serde_json::to_string(&secret_ref).expect("serialize");
        let parsed: SecretRef = serde_json::from_str(&json).expect("deserialize");
        match (parsed, secret_ref) {
            (
                SecretRef::Keychain {
                    service: a_svc,
                    account: a_acc,
                },
                SecretRef::Keychain {
                    service: b_svc,
                    account: b_acc,
                },
            ) => {
                assert_eq!(a_svc, b_svc);
                assert_eq!(a_acc, b_acc);
            }
            (other, _) => panic!("round-trip changed variant: {other:?}"),
        }
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
