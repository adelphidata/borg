//! Tunneling configuration — provider selection + per-provider knobs.
//!
//! Tunnels expose borg's gateway HTTP port to the public internet via an
//! external binary (cloudflared, ngrok, tailscale). The binary is assumed
//! to already be on PATH; missing binaries surface as an actionable error
//! with the install URL when `borg tunnel start` runs.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Top-level tunneling configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelConfig {
    /// Provider selection. `None` is the same as `Provider::None` — no tunnel.
    pub provider: TunnelProvider,
    /// Cloudflare-specific config. Required when `provider = Cloudflare`.
    pub cloudflare: CloudflareTunnelConfig,
    /// Ngrok-specific config. Required when `provider = Ngrok`.
    pub ngrok: NgrokTunnelConfig,
    /// Tailscale-specific config. Defaults are usable as-is for `serve` mode.
    pub tailscale: TailscaleTunnelConfig,
    /// Custom-command config. Required when `provider = Custom`.
    pub custom: CustomTunnelConfig,
}

/// Tunnel provider variants. Display + FromStr give us the case-insensitive
/// string round-trip the settings table needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelProvider {
    /// No tunnel (default).
    #[default]
    None,
    /// `cloudflared` (Cloudflare Zero Trust).
    Cloudflare,
    /// `ngrok`.
    Ngrok,
    /// `tailscale serve` (tailnet-only) or `tailscale funnel` (public).
    Tailscale,
    /// Operator-supplied shell command with `{host}`/`{port}` placeholders.
    Custom,
}

impl FromStr for TunnelProvider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Self::None),
            "cloudflare" | "cloudflared" => Ok(Self::Cloudflare),
            "ngrok" => Ok(Self::Ngrok),
            "tailscale" => Ok(Self::Tailscale),
            "custom" => Ok(Self::Custom),
            other => anyhow::bail!(
                "Unknown tunnel provider '{other}'. Valid: none, cloudflare, ngrok, tailscale, custom"
            ),
        }
    }
}

impl fmt::Display for TunnelProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::None => "none",
            Self::Cloudflare => "cloudflare",
            Self::Ngrok => "ngrok",
            Self::Tailscale => "tailscale",
            Self::Custom => "custom",
        };
        f.write_str(s)
    }
}

/// Cloudflare-tunnel knobs. The token comes from the Cloudflare Zero Trust
/// dashboard and is sensitive; prefer keychain-backed credential storage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudflareTunnelConfig {
    /// Tunnel token from the Cloudflare Zero Trust dashboard.
    /// Resolved via credential store / env when empty.
    pub token: String,
}

/// Ngrok knobs. `auth_token` is required for any non-anonymous tunnel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NgrokTunnelConfig {
    /// `NGROK_AUTHTOKEN` value. Passed via env var to the child process —
    /// never on argv (where `ps`/`tasklist` would expose it).
    pub auth_token: String,
    /// Custom domain. Requires a paid ngrok plan.
    pub domain: Option<String>,
}

/// Tailscale knobs. With Tailscale installed and `tailscale up` already run,
/// defaults yield a tailnet-only `tailscale serve` exposure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TailscaleTunnelConfig {
    /// `true` uses `tailscale funnel --bg` (public HTTPS over Tailscale's
    /// edge). `false` uses `tailscale serve --bg` (tailnet-only).
    pub funnel: bool,
    /// Override the hostname used to build the returned URL. Auto-detected
    /// from `tailscale status --json` when `None`.
    pub hostname: Option<String>,
}

/// Custom-tunnel knobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomTunnelConfig {
    /// Shell command with `{host}` and `{port}` placeholders.
    pub start_command: String,
    /// HTTP endpoint to poll for liveness. `None` skips active healthcheck.
    pub health_url: Option<String>,
    /// Substring to look for in the process's stdout to extract the URL.
    /// `None` skips URL extraction; the operator must point at the tunnel
    /// out-of-band.
    pub url_pattern: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_default_is_none() {
        // Default-on tunneling would silently expose the gateway port at
        // first boot — must always be opt-in.
        assert_eq!(TunnelProvider::default(), TunnelProvider::None);
        assert_eq!(TunnelConfig::default().provider, TunnelProvider::None);
    }

    #[test]
    fn provider_from_str_accepts_canonical_and_aliases_case_insensitively() {
        for (input, expected) in [
            ("none", TunnelProvider::None),
            ("None", TunnelProvider::None),
            ("", TunnelProvider::None),
            ("cloudflare", TunnelProvider::Cloudflare),
            ("Cloudflare", TunnelProvider::Cloudflare),
            ("cloudflared", TunnelProvider::Cloudflare),
            ("ngrok", TunnelProvider::Ngrok),
            ("NGROK", TunnelProvider::Ngrok),
            ("tailscale", TunnelProvider::Tailscale),
            ("custom", TunnelProvider::Custom),
        ] {
            let parsed: TunnelProvider = input.parse().unwrap();
            assert_eq!(parsed, expected, "input={input:?}");
        }
    }

    #[test]
    fn provider_from_str_rejects_unknown_with_actionable_error() {
        // Operators will mistype; the error must list valid options so they
        // don't have to grep source. Asserting on substring rather than
        // full string so future provider additions don't break this test.
        let err = "bogus".parse::<TunnelProvider>().unwrap_err();
        let msg = format!("{err}");
        for must_contain in [
            "bogus",
            "cloudflare",
            "ngrok",
            "tailscale",
            "custom",
            "none",
        ] {
            assert!(
                msg.contains(must_contain),
                "error must mention {must_contain:?}: {msg}"
            );
        }
    }

    #[test]
    fn provider_display_round_trips_via_from_str() {
        for variant in [
            TunnelProvider::None,
            TunnelProvider::Cloudflare,
            TunnelProvider::Ngrok,
            TunnelProvider::Tailscale,
            TunnelProvider::Custom,
        ] {
            let s = variant.to_string();
            let back: TunnelProvider = s.parse().unwrap();
            assert_eq!(back, variant, "round-trip failed for {variant:?}");
        }
    }

    #[test]
    fn serde_lowercases_provider_in_json() {
        // Settings-table JSON storage must be stable across releases — pin
        // the on-disk shape so a rename of the enum variant later cannot
        // silently invalidate existing configs.
        let json = serde_json::to_string(&TunnelProvider::Cloudflare).unwrap();
        assert_eq!(json, "\"cloudflare\"");
    }

    #[test]
    fn ngrok_auth_token_default_is_empty_string_not_option() {
        // Stored as String (not Option<String>) because the settings macro
        // path uses `string` kind which writes "" for unset. Verifying the
        // shape so reviewers don't "fix" it to Option<String> and break the
        // settings_table macro entry.
        let cfg = NgrokTunnelConfig::default();
        assert_eq!(cfg.auth_token, "");
        assert!(cfg.domain.is_none());
    }
}
