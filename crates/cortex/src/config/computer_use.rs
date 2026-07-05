//! `[computer_use]` config — drives the `computer_use` tool.
//!
//! The tool spawns the external `cua-driver` binary over stdio and speaks
//! newline-delimited MCP JSON-RPC for screen capture, app enumeration, and
//! UI control (click/type/drag/…). The settings here are the operator knobs
//! surfaced through `/settings` and the SQLite settings table. The tool
//! defaults on only for macOS (the driver's primary platform); elsewhere the
//! operator opts in once their host has a driver build.

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

impl ComputerUseConfig {
    /// Effective per-phase driver timeout. `0` falls back to the default
    /// (same convention as `mcp.max_response_bytes`) — this is the single
    /// place that fallback lives, so bumping the default can't strand
    /// operators who set `0` on a stale value.
    pub fn driver_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(match self.driver_timeout_secs {
            0 => default_driver_timeout_secs(),
            s => s,
        })
    }
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
        // default to opt-in. Deserialize an empty config (the settings-table
        // path every real boot takes) rather than calling Default directly.
        let cfg: ComputerUseConfig = toml::from_str("").unwrap();
        assert!(
            !cfg.cua_telemetry,
            "cua_telemetry must default off — a true default silently ships usage events to PostHog"
        );
    }

    #[test]
    fn driver_timeout_zero_falls_back_to_default_not_unbounded() {
        // Spec: `0` means "use the default", never "no timeout". An
        // unbounded interpretation would let a wedged driver pin a
        // tool-call slot forever.
        let cfg = ComputerUseConfig {
            driver_timeout_secs: 0,
            ..Default::default()
        };
        assert_eq!(
            cfg.driver_timeout(),
            std::time::Duration::from_secs(default_driver_timeout_secs())
        );
        let explicit = ComputerUseConfig {
            driver_timeout_secs: 90,
            ..Default::default()
        };
        assert_eq!(
            explicit.driver_timeout(),
            std::time::Duration::from_secs(90)
        );
    }
}
