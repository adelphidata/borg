//! Discovery configuration — LAN mDNS announce + Tailscale peer enumeration.
//!
//! Both knobs default to `false` so a fresh install keeps the
//! loopback-only posture. Operators flip them on to enable multi-device
//! pairing without manual hostname wrangling.

use serde::{Deserialize, Serialize};

/// Networked-discovery surface for borgd.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Announce the daemon's mTLS listener as `_borgd._tcp.local` so peers
    /// on the same LAN can find it without manual hostname configuration.
    /// When this is on AND `--listen` is non-loopback, the server-cert SAN
    /// is auto-expanded to include `gethostname()` and `<hostname>.local`
    /// so the announced names verify cleanly.
    pub mdns_enabled: bool,

    /// Override the mDNS instance name. Defaults to `<hostname>-borgd`.
    /// Useful when running multiple borgds on a single host or when the
    /// hostname is ugly.
    pub mdns_instance_name: Option<String>,

    /// Enable `tailscale status --json` peer enumeration for
    /// `borg device pair --discover`. Cheap — only invokes when the
    /// operator asks. Auto-skipped if the `tailscale` binary is missing.
    pub tailscale_enabled: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mdns_enabled: false,
            mdns_instance_name: None,
            tailscale_enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_mdns_off_and_tailscale_on() {
        // mDNS announcement actively changes how the host appears on the
        // network — must be opt-in. Tailscale peer enumeration is read-only
        // and cheap, so it's safe to default-on; the binary's absence is
        // the natural off-switch.
        let cfg = DiscoveryConfig::default();
        assert!(
            !cfg.mdns_enabled,
            "mdns_enabled must default to false (opt-in network announce)"
        );
        assert!(
            cfg.tailscale_enabled,
            "tailscale_enabled defaults true; gated by binary presence"
        );
        assert!(
            cfg.mdns_instance_name.is_none(),
            "instance name defaults to None so the announcer derives it from gethostname()"
        );
    }

    #[test]
    fn serde_roundtrip_preserves_non_default_values() {
        let original = DiscoveryConfig {
            mdns_enabled: true,
            mdns_instance_name: Some("custom-name".into()),
            tailscale_enabled: false,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: DiscoveryConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.mdns_enabled);
        assert_eq!(parsed.mdns_instance_name.as_deref(), Some("custom-name"));
        assert!(!parsed.tailscale_enabled);
    }

    #[test]
    fn deserializing_empty_object_yields_defaults() {
        // The settings loader path treats a missing section as defaults; the
        // serde-default contract must hold or operators get surprising
        // mid-flight discovery toggles when other unrelated sections change.
        let cfg: DiscoveryConfig = serde_json::from_str("{}").unwrap();
        let default = DiscoveryConfig::default();
        assert_eq!(cfg.mdns_enabled, default.mdns_enabled);
        assert_eq!(cfg.tailscale_enabled, default.tailscale_enabled);
        assert_eq!(cfg.mdns_instance_name, default.mdns_instance_name);
    }
}
