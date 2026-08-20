//! LLM provider and credential-pool health checks.

use crate::config::Config;
use crate::doctor::{DiagnosticCheck, RemediationKind};
use crate::llm::provider::Provider;

pub(in crate::doctor) fn check_provider(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    match config.resolve_provider() {
        Ok((provider, key)) => {
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
            } else if provider == Provider::Custom && key.is_empty() {
                // Keyless custom endpoint (llama.cpp, LM Studio, vLLM) —
                // resolution already required `llm.base_url` to be set.
                checks.push(DiagnosticCheck::pass(
                    "Provider",
                    "Custom endpoint configured (keyless)",
                ));
            } else if provider.requires_api_key() {
                checks.push(DiagnosticCheck::pass("Provider", "API key set"));
            } else if provider.uses_aws_credential_chain() {
                // Region resolution already succeeded inside resolve_provider.
                checks.push(DiagnosticCheck::pass(
                    "Provider",
                    "AWS credential chain configured",
                ));
            } else if provider == Provider::Ollama {
                // Probe the endpoint actually in use: `llm.base_url` override
                // → `OLLAMA_HOST` → localhost default. Probing localhost
                // while the operator configured a remote host produced
                // false "not reachable" warnings.
                let base_url = config.llm.base_url.as_deref();
                let origin = crate::llm::provider::ollama_origin(base_url);
                if Provider::ollama_available_at(base_url) {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        format!("Ollama server reachable at {origin}"),
                    ));
                } else {
                    checks.push(DiagnosticCheck::warn(
                        "Provider",
                        "Ollama server",
                        format!("not reachable at {origin} — run `ollama serve`"),
                    ));
                }
            } else {
                // Remaining keyless providers (e.g. Vertex AI cloud OAuth)
                // resolve credentials at request time.
                checks.push(DiagnosticCheck::pass(
                    "Provider",
                    format!("{provider} requires no static API key"),
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
    let pool = crate::llm::credential_pool::try_build_from_keys(provider, keys, &config.llm).await;
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

/// Probe the configured LLM endpoint and report whether it answered.
///
/// Skipped entirely on the unattended path (`probe_network == false`) and for
/// providers that talk to no network endpoint — a Claude-CLI or Bedrock setup
/// has no single base URL to dial, and the checks above already cover them.
pub(in crate::doctor) async fn check_endpoint_reachable(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
    probe_network: bool,
) {
    if !probe_network {
        return;
    }
    let Ok((provider, _key)) = config.resolve_provider() else {
        return; // the Provider check already reports an unresolvable provider
    };
    if matches!(provider, Provider::ClaudeCli) || provider.uses_aws_credential_chain() {
        return;
    }
    let endpoint = config
        .llm
        .base_url
        .clone()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| provider.base_url().to_string());
    if endpoint.trim().is_empty() {
        return;
    }

    // A HEAD with no credentials: we are asking "did anything answer", not
    // "does my key work" — the Credentials check owns that, and sending a key
    // here would put it on the wire for a diagnostic. *Any* HTTP response,
    // including 401/404, proves reachability; only a transport error does not.
    //
    // The shared client, so the probe inherits the same connect timeout, proxy
    // handling and user-agent as real traffic — a probe that succeeded under
    // different settings than the agent uses would be answering the wrong
    // question.
    let failure = crate::http::shared()
        .head(&endpoint)
        .send()
        .await
        .err()
        .map(|e| classify_probe_error(&e));
    checks.push(classify_endpoint_probe(&endpoint, failure));
}

/// How an endpoint probe failed, in the operator's terms rather than the
/// transport's. Each variant maps to a different next action, which is the
/// whole reason for classifying instead of printing the raw error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailure {
    /// The hostname did not resolve.
    Dns,
    /// TLS negotiation failed — usually a corporate MITM certificate that the
    /// trust store does not carry.
    Tls,
    /// A configured proxy refused or could not be reached.
    Proxy,
    /// Connected but the endpoint never answered.
    Timeout,
    /// Reached the network and got a refusal — nothing listening, or blocked.
    Refused,
    /// Anything the shapes above do not cover.
    Other,
}

impl ProbeFailure {
    /// The next action, in plain words. Borg's UX invariant: never hand back a
    /// transport error and leave the operator to decode it.
    fn remedy(self) -> &'static str {
        match self {
            Self::Dns => "the endpoint hostname did not resolve — check the address and your DNS",
            Self::Tls => {
                "TLS failed — if your network inspects traffic, add its CA to the system trust store"
            }
            Self::Proxy => "the configured proxy could not be reached — check HTTPS_PROXY",
            Self::Timeout => "the endpoint accepted the connection but never answered — retry, or check whether it is blocked",
            Self::Refused => "nothing answered at that address — check the endpoint URL and any firewall",
            Self::Other => "the endpoint could not be reached",
        }
    }
}

/// Classify a transport error. Ordering matters: a proxy failure also reports
/// as a connect error, and a TLS failure inside a proxy tunnel reports as both,
/// so the more specific cause is tested first.
fn classify_probe_error(err: &reqwest::Error) -> ProbeFailure {
    // Typed answers first, because they cannot be wrong.
    if err.is_timeout() {
        return ProbeFailure::Timeout;
    }

    // `reqwest` exposes no typed variant for the rest, so the rendered chain is
    // the only signal available. Only the *causes* are rendered, never the
    // top-level error: its `Debug` embeds the request URL, so an operator whose
    // endpoint is `https://proxy.corp.net/v1` or `https://dns.example/v1` would
    // be told to check their proxy or their resolver no matter what actually
    // went wrong — the precise mis-advice this classification exists to avoid.
    let mut text = String::new();
    let mut cause = std::error::Error::source(err);
    while let Some(source) = cause {
        text.push_str(&source.to_string());
        text.push(' ');
        cause = source.source();
    }
    let text = text.to_ascii_lowercase();
    if text.contains("proxy") {
        ProbeFailure::Proxy
    } else if text.contains("certificate")
        || text.contains("tls")
        || text.contains("handshake")
        || text.contains("invalidcertificate")
    {
        ProbeFailure::Tls
    } else if text.contains("dns")
        || text.contains("name or service not known")
        || text.contains("failed to lookup address")
        || text.contains("nodename nor servname")
    {
        ProbeFailure::Dns
    } else if text.contains("connection refused") || err.is_connect() {
        ProbeFailure::Refused
    } else {
        ProbeFailure::Other
    }
}

/// Turn a probe outcome into a row. Reachability is a `Warn`, never a `Fail`:
/// a laptop that is briefly offline is not a broken install, and the Provider
/// and Credentials checks above already fail loudly for real misconfiguration.
fn classify_endpoint_probe(endpoint: &str, failure: Option<ProbeFailure>) -> DiagnosticCheck {
    match failure {
        None => DiagnosticCheck::pass("Provider", format!("endpoint reachable ({endpoint})")),
        Some(failure) => DiagnosticCheck::warn(
            "Provider",
            "endpoint reachable",
            format!("{endpoint}: {}", failure.remedy()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

    #[test]
    fn endpoint_probe_names_a_distinct_action_per_failure_shape() {
        // The point of classifying is that each cause has a *different* next
        // action; collapsing two of them into one message would silently send
        // the operator down the wrong path (e.g. editing DNS when the real
        // problem is a corporate TLS interceptor).
        let all = [
            ProbeFailure::Dns,
            ProbeFailure::Tls,
            ProbeFailure::Proxy,
            ProbeFailure::Timeout,
            ProbeFailure::Refused,
            ProbeFailure::Other,
        ];
        let mut seen: Vec<&str> = Vec::new();
        for failure in all {
            let check = classify_endpoint_probe("https://api.example.com", Some(failure));
            let CheckStatus::Warn(message) = &check.status else {
                panic!("an unreachable endpoint must warn, not fail or pass: {check:?}");
            };
            assert!(
                message.contains("https://api.example.com"),
                "message must name the endpoint probed: {message}"
            );
            let remedy = failure.remedy();
            assert!(
                !seen.contains(&remedy),
                "{failure:?} repeats another failure's advice: {remedy}"
            );
            seen.push(remedy);
        }

        let ok = classify_endpoint_probe("https://api.example.com", None);
        assert!(matches!(ok.status, CheckStatus::Pass));
    }

    #[tokio::test]
    async fn a_dead_endpoint_classifies_as_refused_not_as_a_vague_other() {
        // Driven through a real `reqwest` failure rather than a hand-built
        // string, so the substring matching above is checked against what the
        // library actually renders — the part most likely to rot silently when
        // reqwest or its resolver changes an error's wording.
        //
        // Loopback port 1 only: the stack is reachable and nothing is
        // listening, so this needs no DNS and no network, and behaves the same
        // on a disconnected machine. The DNS/TLS/proxy arms cannot be driven
        // hermetically — exercising them would mean a real resolver or a real
        // handshake, which is what the opt-in live lanes are for.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(250))
            .build()
            .expect("build probe client");

        let refused = client
            .get("http://127.0.0.1:1/v1")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert_eq!(classify_probe_error(&refused), ProbeFailure::Refused);
    }

    #[tokio::test]
    async fn the_endpoint_url_never_decides_the_diagnosis() {
        // Regression: the classifier used to search the top-level error's
        // `Debug`, which embeds the request URL. An operator running behind a
        // gateway literally named "proxy", or a provider on a host with "dns"
        // or "tls" in it, then got advice for a problem they did not have —
        // and the real cause (nothing listening) was never reported. The words
        // live in the path here so the case needs no name resolution.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(250))
            .build()
            .expect("build probe client");

        let err = client
            .get("http://127.0.0.1:1/proxy/tls/certificate/dns/handshake")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert_eq!(
            classify_probe_error(&err),
            ProbeFailure::Refused,
            "the cause decides the diagnosis, not the spelling of the endpoint"
        );
    }

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
