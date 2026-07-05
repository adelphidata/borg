//! `[computer_use]` config — drives the macOS-only `computer_use` tool.
//!
//! The tool spawns the external `cua-driver` binary over stdio and speaks a
//! framed JSON-RPC dialect for screen capture, app enumeration, and (in the
//! mutating-actions commit) UI control. The settings here are the operator
//! knobs surfaced through `/settings` and the SQLite settings table.

use serde::{Deserialize, Serialize};

/// Tunables for the `computer_use` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComputerUseConfig {
    /// Master switch. `true` on macOS by default; `false` everywhere else
    /// because the driver currently only ships for macOS.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Absolute path to the `cua-driver` binary. Empty means "search `PATH`",
    /// which is the default. Set this when the driver lives outside `PATH`
    /// (for example a per-user `~/.local/bin` install).
    #[serde(default)]
    pub driver_path: String,

    /// Opt-in for cua-driver's anonymous usage telemetry (PostHog). The
    /// driver ships with telemetry ON upstream; Borg disables it by
    /// injecting `CUA_DRIVER_RS_TELEMETRY_ENABLED=0` into the child env
    /// unless the operator flips this to `true` (then the var is left unset
    /// so the driver applies its own default).
    #[serde(default)]
    pub cua_telemetry: bool,

    /// Per-phase timeout for one driver round-trip (spawn → initialize →
    /// tools/call), in seconds. Guards against a wedged driver pinning a
    /// tool-call slot forever. `0` falls back to the default rather than
    /// meaning "unbounded".
    #[serde(default = "default_driver_timeout_secs")]
    pub driver_timeout_secs: u64,
}

/// 30s mirrors the driver's worst observed cold-start (first-run permission
/// prompts + AX cache warm) with headroom; normal calls complete in <2s.
const fn default_driver_timeout_secs() -> u64 {
    30
}

#[cfg(target_os = "macos")]
const fn default_enabled() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
const fn default_enabled() -> bool {
    false
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            driver_path: String::new(),
            cua_telemetry: false,
            driver_timeout_secs: default_driver_timeout_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled_matches_platform() {
        let cfg = ComputerUseConfig::default();
        if cfg!(target_os = "macos") {
            assert!(
                cfg.enabled,
                "computer_use must default-on for macOS so the tool is available without operator intervention"
            );
        } else {
            assert!(
                !cfg.enabled,
                "computer_use must default-off on non-macOS — the driver is macOS-only"
            );
        }
    }

    #[test]
    fn default_driver_path_is_empty() {
        // Empty means "search PATH" — the documented contract operators rely
        // on when they install cua-driver via homebrew or a package manager.
        let cfg = ComputerUseConfig::default();
        assert!(cfg.driver_path.is_empty());
    }

    #[test]
    fn deserializes_from_partial_toml() {
        // Operators typically set one field, not both; serde defaults must
        // fill the rest so the apply_setting hot-path doesn't need to know
        // about every field.
        let cfg: ComputerUseConfig =
            toml::from_str(r#"driver_path = "/usr/local/bin/cua-driver""#).unwrap();
        assert_eq!(cfg.driver_path, "/usr/local/bin/cua-driver");
        assert_eq!(cfg.enabled, default_enabled());
        assert_eq!(cfg.driver_timeout_secs, 30);
    }

    #[test]
    fn telemetry_defaults_off() {
        // Privacy spec: cua-driver ships telemetry ON upstream; Borg must
        // default to opt-in. A `true` default here would silently ship
        // usage events to PostHog for every operator.
        assert!(!ComputerUseConfig::default().cua_telemetry);
    }
}
