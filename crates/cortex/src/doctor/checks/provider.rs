//! LLM provider and credential-pool health checks.

use crate::config::Config;
use crate::doctor::{DiagnosticCheck, RemediationKind};
use crate::llm::provider::Provider;

pub(in crate::doctor) fn check_provider(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    match config.resolve_provider() {
        Ok((provider, _key)) => {
            if provider == Provider::ClaudeCli {
                if let Some(path) = crate::claude_cli::detect_cli_path() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        format!("Claude CLI found at {}", path.display()),
                    ));
                } else {
                    checks.push(DiagnosticCheck::fail(
                        "Provider",
                        "Claude CLI binary",
                        "not found — install Claude Code or set CLAUDE_CLI_PATH",
                    ));
                }
                if crate::claude_cli::has_valid_auth() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        "Claude CLI authenticated",
                    ));
                } else {
                    checks.push(DiagnosticCheck::warn(
                        "Provider",
                        "Claude CLI auth",
                        "not authenticated or expired — run `claude login`",
                    ));
                }
            } else if provider.requires_api_key() {
                checks.push(DiagnosticCheck::pass("Provider", "API key set"));
            } else if Provider::ollama_available() {
                checks.push(DiagnosticCheck::pass("Provider", "Ollama server reachable"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Provider",
                    "Ollama server",
                    format!(
                        "not reachable at localhost:{} — run `ollama serve`",
                        crate::constants::OLLAMA_PORT_DEFAULT
                    ),
                ));
            }
            checks.push(DiagnosticCheck::pass(
                "Provider",
                format!("Provider: {provider}"),
            ));
            check_model_deprecation(provider, &config.llm.model, checks);
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Provider",
                "API key set",
                format!("{e}"),
            ));
        }
    }

    // Also report Claude CLI availability even if not the active provider
    if config
        .llm
        .provider
        .as_deref()
        .is_none_or(|p| p != "claude-cli")
        && crate::claude_cli::detect_cli_path().is_some()
        && crate::claude_cli::has_valid_auth()
    {
        checks.push(DiagnosticCheck::pass(
            "Provider",
            "Claude CLI available (alternative provider)",
        ));
    }
}

/// If the active `(provider, model)` is in the deprecation table, emit a
/// warning that names the retirement date and recommended replacement so
/// operators can migrate before the upstream provider starts rejecting
/// requests.
fn check_model_deprecation(provider: Provider, model: &str, checks: &mut Vec<DiagnosticCheck>) {
    let Some(entry) = crate::llm::deprecations::lookup(provider, model) else {
        return;
    };
    let msg = format!(
        "{} retires on {} — switch to `{}` before that date",
        entry.model, entry.retires_on, entry.replacement,
    );
    let mut check = DiagnosticCheck::warn("Provider", "Model retirement", msg);
    check.user_hint = Some(format!(
        "Set `llm.model` to `{}`. Provider advisory: {}",
        entry.replacement, entry.advisory_url
    ));
    check.remediation_kind = RemediationKind::Manual;
    check.remediation_key = Some("provider.model_retirement");
    checks.push(check);
}

/// Pure pass/warn/fail decision for credential-pool health. Split out so the
/// thresholds are testable without constructing a real pool. `has_credentials`
/// false → Fail (nothing usable); available → Pass; configured-but-all-cooling
/// → Warn with the earliest recovery estimate.
fn classify_credential_pool(
    provider_name: &str,
    has_credentials: bool,
    has_available: bool,
    next_recovery_at_ms: Option<i64>,
    now_ms: i64,
) -> DiagnosticCheck {
    if !has_credentials {
        return DiagnosticCheck::fail(
            "Credentials",
            format!("{provider_name} credentials"),
            "no usable API credentials resolved for the active provider",
        );
    }
    if has_available {
        return DiagnosticCheck::pass(
            "Credentials",
            format!("{provider_name} credentials available"),
        );
    }
    let detail = match next_recovery_at_ms {
        Some(reset_ms) if reset_ms > now_ms => {
            format!(
                "all keys rate-limited/cooling — earliest recovery in ~{}s",
                (reset_ms - now_ms) / 1000
            )
        }
        _ => "all keys exhausted with no scheduled recovery".to_string(),
    };
    DiagnosticCheck::warn(
        "Credentials",
        format!("{provider_name} credentials"),
        detail,
    )
}

/// Whether the credential-pool check applies to `provider`: only providers
/// that authenticate with a plaintext API key have an inspectable pool. Local
/// (Ollama), CLI, and AWS-credential-chain providers have no pool and are
/// covered by the Provider check. Pure, so the applicability gate is testable
/// alongside the severity mapping in `classify_credential_pool`.
fn credential_check_applies(provider: Provider) -> bool {
    provider.requires_api_key() && !provider.uses_aws_credential_chain()
}

/// LLM credential-pool health (offline). Applicable only to key-authenticated
/// providers (see [`credential_check_applies`]); other providers are covered by
/// the Provider check.
pub(in crate::doctor) async fn check_llm_credentials(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
) {
    let (provider, keys) = match config.resolve_api_keys() {
        Ok((p, keys)) if credential_check_applies(p) => (p, keys),
        _ => return,
    };
    if keys.iter().all(String::is_empty) {
        return; // nothing resolvable — the Provider check already reports this
    }
    let pool = crate::llm::credential_pool::try_build_from_keys(
        provider,
        keys,
        config.llm.credential_strategy,
        config.llm.credential_cooldown_secs,
        config.llm.max_concurrent_per_credential,
        config.llm.proactive_rate_limit_threshold_pct,
        config.llm.proactive_rate_limit_min_window_secs,
    )
    .await;
    match pool {
        Some(pool) => checks.push(classify_credential_pool(
            provider.as_str(),
            pool.has_credentials(),
            pool.has_available(),
            pool.next_recovery_at_ms(),
            chrono::Utc::now().timestamp_millis(),
        )),
        None => checks.push(DiagnosticCheck::warn(
            "Credentials",
            format!("{} credential pool", provider.as_str()),
            "could not load credential pool (see logs)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

    #[test]
    fn credential_pool_classification_covers_all_states() {
        let no_creds = classify_credential_pool("openrouter", false, false, None, 0);
        assert!(
            matches!(no_creds.status, CheckStatus::Fail(_)),
            "no credentials → fail"
        );

        let ok = classify_credential_pool("openrouter", true, true, None, 0);
        assert!(matches!(ok.status, CheckStatus::Pass), "available → pass");

        let cooling = classify_credential_pool("openrouter", true, false, Some(61_000), 1_000);
        assert!(
            matches!(cooling.status, CheckStatus::Warn(_)),
            "configured but all cooling → warn"
        );
        let CheckStatus::Warn(msg) = &cooling.status else {
            unreachable!()
        };
        assert!(msg.contains("60s"), "warn must estimate recovery: {msg}");
    }

    #[test]
    fn credential_check_applies_only_to_key_providers() {
        // Key-authenticated cloud provider → applicable.
        assert!(credential_check_applies(Provider::OpenRouter));
        // Local provider has no key pool → not applicable.
        assert!(!credential_check_applies(Provider::Ollama));
        // AWS-credential-chain provider resolves creds elsewhere → not applicable.
        assert!(!credential_check_applies(Provider::Bedrock));
    }
}
