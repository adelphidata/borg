//! Network-facing subsystem checks: gateway channels, tunnel providers, and
//! MCP server reachability.

use crate::config::tunnel::TunnelProvider;
use crate::config::Config;
use crate::doctor::{DiagnosticCheck, RemediationKind};

pub(in crate::doctor) fn check_gateway(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Gateway",
        format!(
            "gateway config: {}:{}",
            config.gateway.host, config.gateway.port
        ),
    ));

    match Config::channels_dir() {
        Ok(channels_dir) => {
            if channels_dir.exists() {
                let mut count = 0;
                let mut errors = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&channels_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let manifest_path = path.join("channel.toml");
                            if manifest_path.exists() {
                                match std::fs::read_to_string(&manifest_path) {
                                    Ok(content) => match toml::from_str::<toml::Value>(&content) {
                                        Ok(_) => count += 1,
                                        Err(e) => {
                                            errors.push(format!("{}: {e}", manifest_path.display()))
                                        }
                                    },
                                    Err(e) => {
                                        errors.push(format!("{}: {e}", manifest_path.display()))
                                    }
                                }
                            }
                        }
                    }
                }

                checks.push(DiagnosticCheck::pass(
                    "Gateway",
                    format!("{count} channel(s) found"),
                ));

                for error in errors {
                    checks.push(DiagnosticCheck::fail("Gateway", "channel manifest", error));
                }
            } else {
                // Fresh install: ~/.borg/channels is created lazily when a
                // script channel is installed. No directory means no script
                // channels, which is the normal state — not a warning.
                checks.push(DiagnosticCheck::pass("Gateway", "0 channel(s) found"));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Gateway",
                "channels directory",
                format!("{e}"),
            ));
        }
    }
}

/// External binary a tunnel provider needs, if any. `None` (no tunnel) and
/// `Custom` (operator-supplied command) have no fixed binary requirement.
fn tunnel_binary(provider: TunnelProvider) -> Option<&'static str> {
    match provider {
        TunnelProvider::Cloudflare => Some("cloudflared"),
        TunnelProvider::Ngrok => Some("ngrok"),
        TunnelProvider::Tailscale => Some("tailscale"),
        TunnelProvider::None | TunnelProvider::Custom => None,
    }
}

/// Pure severity mapping for the tunnel check, given the provider's already-
/// resolved `bin` (applicability — does the provider need a binary — is gated by
/// `check_tunnel`). Binary missing from PATH → Fail; present → Pass.
fn classify_tunnel(provider: TunnelProvider, bin: &str, binary_present: bool) -> DiagnosticCheck {
    if binary_present {
        DiagnosticCheck::pass("Tunnel", format!("{provider} binary `{bin}` found"))
    } else {
        let mut c = DiagnosticCheck::fail(
            "Tunnel",
            format!("{provider} binary"),
            format!("`{bin}` not found on PATH — `borg tunnel start` will fail"),
        );
        c.remediation_kind = RemediationKind::Manual;
        c.user_hint = Some(format!("install `{bin}` and ensure it is on PATH"));
        c
    }
}

pub(in crate::doctor) fn check_tunnel(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let provider = config.tunnel.provider;
    if let Some(bin) = tunnel_binary(provider) {
        checks.push(classify_tunnel(provider, bin, which::which(bin).is_ok()));
    }
}

/// Build the per-server MCP check from its lifecycle label. Pure so the
/// state→severity mapping is testable without a live registry — which the CLI
/// `borg doctor` doesn't have, so every server reads as "not-started" there.
/// That state must NOT be a failure (configured ≠ broken).
fn mcp_check_for_label(name: &str, label: &str) -> DiagnosticCheck {
    match label {
        "connected" => DiagnosticCheck::pass("MCP", format!("{name}: connected")),
        "auth-failed" => DiagnosticCheck::fail(
            "MCP",
            format!("{name}: auth"),
            "authentication failed — re-pair the server's credentials",
        ),
        "reconnecting" => DiagnosticCheck::warn(
            "MCP",
            format!("{name}: reconnecting"),
            "transport dropped — retrying",
        ),
        "degraded" => DiagnosticCheck::warn(
            "MCP",
            format!("{name}: degraded"),
            "server is up but some tools were rejected",
        ),
        // "not-started" / "disabled" / "live-disabled": informational only.
        other => DiagnosticCheck::pass("MCP", format!("{name}: {other}")),
    }
}

/// MCP server reachability. Applicable only when servers are configured;
/// suppressed otherwise. Per server, maps its live lifecycle state to a check.
pub(in crate::doctor) fn check_mcp(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    for snap in crate::mcp_clients::server_status_snapshot(config) {
        checks.push(mcp_check_for_label(&snap.name, snap.state_label()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fails, passes, warns};
    use super::*;

    #[test]
    fn tunnel_classification_gates_on_provider_and_binary() {
        // Applicability lives in tunnel_binary: no tunnel / custom command have
        // no fixed binary, so check_tunnel emits nothing for them.
        assert!(tunnel_binary(TunnelProvider::None).is_none());
        assert!(tunnel_binary(TunnelProvider::Custom).is_none());
        assert_eq!(
            tunnel_binary(TunnelProvider::Cloudflare),
            Some("cloudflared")
        );
        // Provider binary missing → fail; present → pass.
        let missing = classify_tunnel(TunnelProvider::Ngrok, "ngrok", false);
        assert!(fails(&missing));
        assert_eq!(missing.remediation_kind, RemediationKind::Manual);
        assert!(passes(&classify_tunnel(
            TunnelProvider::Ngrok,
            "ngrok",
            true
        )));
    }

    #[test]
    fn mcp_label_maps_states_without_false_positives() {
        assert!(fails(&mcp_check_for_label("srv", "auth-failed")));
        assert!(warns(&mcp_check_for_label("srv", "reconnecting")));
        assert!(warns(&mcp_check_for_label("srv", "degraded")));
        assert!(passes(&mcp_check_for_label("srv", "connected")));
        // The CLI has no live registry — "not-started" must NOT be a problem.
        assert!(passes(&mcp_check_for_label("srv", "not-started")));
    }

    #[test]
    fn mcp_check_emits_nothing_without_configured_servers() {
        let mut checks = Vec::new();
        check_mcp(&Config::default(), &mut checks);
        assert!(
            checks.is_empty(),
            "no configured MCP servers → no rows (suppressed section)",
        );
    }
}
