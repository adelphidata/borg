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
    }
}
