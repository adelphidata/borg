//! Security posture checks: secrets hygiene, KEK backend, config security
//! posture, and dependency advisories.

use crate::config::security::KekBackend;
use crate::config::Config;
use crate::doctor::{DiagnosticCheck, RemediationKind};

pub(in crate::doctor) fn check_secrets(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if config.llm.api_key.is_some() {
        checks.push(DiagnosticCheck::pass("Secrets", "API key via SecretRef"));
    }

    if let Ok(data_dir) = Config::data_dir() {
        let env_path = data_dir.join(".env");
        if env_path.exists() {
            match std::fs::read_to_string(&env_path) {
                Ok(contents) => {
                    let has_plaintext_keys = contents.lines().any(|line| {
                        let trimmed = line.trim();
                        !trimmed.is_empty()
                            && !trimmed.starts_with('#')
                            && trimmed.contains('=')
                            && (trimmed.contains("API_KEY") || trimmed.contains("api_key"))
                    });

                    if has_plaintext_keys {
                        let hint = if config.llm.api_key.is_some() {
                            "plaintext .env still present — consider removing it"
                        } else {
                            "plaintext API keys in .env — move them into the OS keychain and set llm.api_key via a SecretRef keychain source in settings"
                        };
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            "plaintext keys in .env",
                            hint,
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass(
                            "Secrets",
                            "no plaintext API keys in .env",
                        ));
                    }
                }
                Err(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Secrets",
                        ".env readable",
                        "could not read .env file",
                    ));
                }
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = std::fs::metadata(&env_path) {
                    let mode = meta.mode() & 0o777;
                    if mode & 0o077 != 0 {
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            ".env file permissions",
                            format!("permissions are {mode:04o} — should be 0600 or stricter"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass("Secrets", ".env file permissions"));
                    }
                }
            }
        }
    }

    if !config.llm.api_keys.is_empty() {
        let resolved_count = config
            .llm
            .api_keys
            .iter()
            .filter(|sr| sr.resolve().is_ok())
            .count();
        let name = format!(
            "multi-key fallback: {resolved_count}/{} keys resolvable",
            config.llm.api_keys.len()
        );
        if resolved_count > 0 {
            checks.push(DiagnosticCheck::pass("Secrets", name));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Secrets",
                name,
                "no fallback keys could be resolved",
            ));
        }
    }
}

/// Pure decision for the secrets-backend posture check. `kek_file_present` only
/// matters for the `File` backend, whose encrypted blob must exist.
fn classify_keychain(backend: KekBackend, kek_file_present: bool) -> DiagnosticCheck {
    match backend {
        KekBackend::OsKeychain => DiagnosticCheck::pass("Keychain", "KEK backend: OS keychain"),
        // `Auto` resolves to a concrete backend on first daemon start and
        // persists it; seeing it here just means the daemon hasn't booted yet.
        KekBackend::Auto => {
            DiagnosticCheck::pass("Keychain", "KEK backend: auto (resolves on daemon start)")
        }
        KekBackend::File if kek_file_present => DiagnosticCheck::warn(
            "Keychain",
            "KEK backend: encrypted file",
            "file backend in use — posture is Reduced (×0.85). `borg service install --secure` upgrades to an OS keychain",
        ),
        KekBackend::File => DiagnosticCheck::fail(
            "Keychain",
            "KEK backend: encrypted file",
            "file backend selected but ~/.borg/secrets/kek.enc is missing — the daemon cannot unseal its keys",
        ),
    }
}

/// Secrets/keychain posture. Config-based (cortex doesn't link `identity`, so
/// no live keychain round-trip): reports the resolved KEK backend and, for the
/// file backend, whether its encrypted blob is present. Always applicable.
pub(in crate::doctor) fn check_keychain(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let kek_present = Config::data_dir()
        .map(|home| crate::paths::kek_file_path(&home).exists())
        .unwrap_or(false);
    checks.push(classify_keychain(config.security.kek_backend, kek_present));
}

pub(in crate::doctor) fn check_config_security(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    use crate::pairing::DmPolicy;

    match config.gateway.dm_policy {
        DmPolicy::Open => checks.push(DiagnosticCheck::warn(
            "Security",
            "DM policy",
            "set to \"open\" — all senders can interact without approval. Set to \"pairing\" for access control",
        )),
        DmPolicy::Pairing => checks.push(DiagnosticCheck::pass("Security", "DM policy (pairing)")),
        DmPolicy::Disabled => checks.push(DiagnosticCheck::pass("Security", "DM policy (disabled)")),
    }

    // Per-channel open policies (dynamic list — not table-driven)
    let open_channels: Vec<&String> = config
        .gateway
        .channel_policies
        .iter()
        .filter(|(_, policy)| matches!(policy, DmPolicy::Open))
        .map(|(name, _)| name)
        .collect();
    if open_channels.is_empty() {
        checks.push(DiagnosticCheck::pass(
            "Security",
            "channel policies (no open overrides)",
        ));
    } else {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "channel policies",
            format!(
                "open DM policy on: {}. Consider \"pairing\" for untrusted channels",
                open_channels
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // Table-driven boolean/simple checks
    struct BoolCheck {
        ok: bool,
        pass_label: &'static str,
        warn_label: &'static str,
        warn_detail: &'static str,
    }

    let bool_checks = [
        BoolCheck {
            ok: config.sandbox.enabled && config.sandbox.mode != "permissive",
            pass_label: if config.sandbox.enabled { "sandbox enabled (strict)" } else { "sandbox" },
            warn_label: if !config.sandbox.enabled { "sandbox" } else { "sandbox mode" },
            warn_detail: if !config.sandbox.enabled {
                "disabled — user tools run without isolation. Set sandbox.enabled = true"
            } else {
                "set to \"permissive\" — weaker isolation. Consider \"strict\""
            },
        },
        BoolCheck {
            ok: config.security.secret_detection,
            pass_label: "secret detection enabled",
            warn_label: "secret detection",
            warn_detail: "disabled — API keys and tokens may leak in tool output. Set security.secret_detection = true",
        },
        BoolCheck {
            ok: !config.security.blocked_paths.is_empty(),
            pass_label: "blocked paths",
            warn_label: "blocked paths",
            warn_detail: "empty — sensitive directories (.ssh, .aws, .gnupg) are not protected. Add entries to security.blocked_paths",
        },
        BoolCheck {
            ok: config.security.action_limits.tool_calls_block <= 1000
                && config.security.action_limits.shell_commands_block <= 500
                && config.security.action_limits.file_writes_block <= 300,
            pass_label: "rate limits within bounds",
            warn_label: "rate limits",
            warn_detail: "block thresholds are very high — consider lowering for tighter safety bounds",
        },
        BoolCheck {
            ok: config.budget.monthly_token_limit != 0,
            pass_label: "budget",
            warn_label: "budget",
            warn_detail: "unlimited (monthly_token_limit = 0) — no spend cap. Set a limit to prevent runaway usage",
        },
    ];

    for check in &bool_checks {
        if check.ok {
            // Dynamic pass labels for blocked_paths count and budget cap
            let label = match check.pass_label {
                "blocked paths" => format!(
                    "blocked paths ({} entries)",
                    config.security.blocked_paths.len()
                ),
                "budget" => format!(
                    "budget capped ({} tokens/month)",
                    config.budget.monthly_token_limit
                ),
                other => other.to_string(),
            };
            checks.push(DiagnosticCheck::pass("Security", label));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Security",
                check.warn_label,
                check.warn_detail,
            ));
        }
    }

    if config.browser.enabled && config.browser.no_sandbox {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "browser sandbox",
            "Chrome --no-sandbox is enabled — browser runs without isolation. Set browser.no_sandbox = false",
        ));
    } else if config.browser.enabled {
        checks.push(DiagnosticCheck::pass("Security", "browser sandbox enabled"));
    }
}

/// Compare each shipped dependency against the bundled advisory list. A
/// single check row per advisory hit, so operators can squelch noisy entries
/// one at a time. No I/O — both the lockfile and the advisory list are
/// `include_str!`-embedded at compile time.
pub(in crate::doctor) fn check_supply_chain(checks: &mut Vec<DiagnosticCheck>) {
    let hits = crate::security::advisory::scan_embedded();
    if hits.is_empty() {
        checks.push(DiagnosticCheck::pass(
            "Supply Chain",
            "Dependency advisories",
        ));
        return;
    }
    for hit in hits {
        let name = format!(
            "{} {} ({})",
            hit.advisory.crate_name, hit.installed_version, hit.advisory.id
        );
        let msg = hit.advisory.summary.clone();
        let mut check = if hit.advisory.severity.is_failing() {
            DiagnosticCheck::fail("Supply Chain", name, msg)
        } else {
            DiagnosticCheck::warn("Supply Chain", name, msg)
        };
        check.user_hint = Some(hit.advisory.fix.clone());
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("supply_chain.advisory");
        checks.push(check);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fails, passes, warns};
    use super::*;
    use crate::doctor::CheckStatus;

    #[test]
    fn keychain_classification_reflects_backend_and_file_presence() {
        assert!(passes(&classify_keychain(KekBackend::OsKeychain, false)));
        assert!(passes(&classify_keychain(KekBackend::Auto, false)));
        assert!(
            warns(&classify_keychain(KekBackend::File, true)),
            "file backend with blob present → reduced-posture warning",
        );
        assert!(
            fails(&classify_keychain(KekBackend::File, false)),
            "file backend with missing blob → fail (cannot unseal)",
        );
    }

    #[test]
    fn config_security_secure_defaults_all_pass() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        // All checks should be in the Security category
        assert!(checks.iter().all(|c| c.category == "Security"));
        // Default config is secure — no warnings expected
        let warns: Vec<_> = checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Warn(_)))
            .collect();
        assert!(
            warns.is_empty(),
            "Secure defaults should produce no warnings: {warns:?}"
        );
    }

    #[test]
    fn config_security_open_dm_policy_warns() {
        let mut config = Config::default();
        config.gateway.dm_policy = crate::pairing::DmPolicy::Open;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let dm_check = checks.iter().find(|c| c.name == "DM policy").unwrap();
        assert!(matches!(dm_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_sandbox_disabled_warns() {
        let mut config = Config::default();
        config.sandbox.enabled = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let sandbox_check = checks.iter().find(|c| c.name == "sandbox").unwrap();
        assert!(matches!(sandbox_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_secret_detection_off_warns() {
        let mut config = Config::default();
        config.security.secret_detection = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "secret detection")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_empty_blocked_paths_warns() {
        let mut config = Config::default();
        config.security.blocked_paths.clear();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "blocked paths").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_browser_no_sandbox_warns() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.no_sandbox = true;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "browser sandbox").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_open_channel_policy_warns() {
        let mut config = Config::default();
        config
            .gateway
            .channel_policies
            .insert("telegram".into(), crate::pairing::DmPolicy::Open);
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "channel policies")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(ref msg) if msg.contains("telegram")));
    }

    #[test]
    fn config_security_high_rate_limits_warns() {
        let mut config = Config::default();
        config.security.action_limits.tool_calls_block = 1500;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "rate limits").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_budget_set_passes() {
        let mut config = Config::default();
        config.budget.monthly_token_limit = 1_000_000;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name.contains("budget")).unwrap();
        assert!(matches!(check.status, CheckStatus::Pass));
    }
}
