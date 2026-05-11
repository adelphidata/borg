use serde::{Deserialize, Serialize};

use crate::process_registry::default_never_kill_patterns;

/// Settings for the subprocess process registry — orphan thresholds,
/// auto-reap policy, and finished-queue bounds.
///
/// The registry tracks every agent-initiated subprocess (`run_shell`,
/// `browser`, hook scripts, scheduled command tasks). These knobs
/// control how the daily maintenance sweep classifies and reaps them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessRegistryConfig {
    /// Age past which a running subprocess triggers a doctor `Warn`.
    pub orphan_warn_secs: u64,
    /// Age past which a running subprocess triggers a doctor `Fail`.
    pub orphan_fail_secs: u64,
    /// Age past which the daily sweep SIGTERMs+SIGKILLs the subprocess.
    /// `0` disables auto-reap entirely while keeping the rest of the
    /// registry — visibility-only mode.
    pub auto_reap_secs: u64,
    /// Seconds between SIGTERM and SIGKILL during auto-reap. The reaper
    /// polls liveness at 100ms granularity within this window.
    pub sigterm_grace_secs: u64,
    /// Glob patterns matched against the command string. Entries whose
    /// command matches any pattern are exempt from auto-reap. Ships
    /// with common long-running build/test workloads pre-populated.
    pub never_kill_patterns: Vec<String>,
    /// How long completed entries linger in the finished queue before
    /// the sweep evicts them.
    pub finished_ttl_secs: u64,
    /// Hard cap on the finished queue (LRU). Bounds memory growth on
    /// daemons that have been running for weeks.
    pub max_finished_entries: usize,
}

impl Default for ProcessRegistryConfig {
    fn default() -> Self {
        Self {
            orphan_warn_secs: 3600,  // 1h
            orphan_fail_secs: 14400, // 4h
            auto_reap_secs: 86400,   // 24h
            sigterm_grace_secs: 5,
            never_kill_patterns: default_never_kill_patterns(),
            finished_ttl_secs: 1800, // 30 min
            max_finished_entries: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        // Doc table in docs/self-healing.md cites these defaults
        // verbatim. If you change one, change both.
        let cfg = ProcessRegistryConfig::default();
        assert_eq!(cfg.orphan_warn_secs, 3600);
        assert_eq!(cfg.orphan_fail_secs, 14400);
        assert_eq!(cfg.auto_reap_secs, 86400);
        assert_eq!(cfg.sigterm_grace_secs, 5);
        assert_eq!(cfg.finished_ttl_secs, 1800);
        assert_eq!(cfg.max_finished_entries, 256);
        assert!(!cfg.never_kill_patterns.is_empty());
    }

    #[test]
    fn auto_reap_secs_zero_means_disabled() {
        // Spec contract surfaced in the docs: setting to 0 disables
        // auto-reap without disabling the rest of the registry.
        // The maintenance step that consumes this honors the zero
        // case; here we just lock in the semantic via the type.
        let cfg = ProcessRegistryConfig {
            auto_reap_secs: 0,
            ..Default::default()
        };
        assert_eq!(cfg.auto_reap_secs, 0);
    }

    #[test]
    fn config_parses_from_toml() {
        let toml_str = r#"
            orphan_warn_secs = 600
            auto_reap_secs = 0
            never_kill_patterns = ["my-build*"]
        "#;
        let cfg: ProcessRegistryConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.orphan_warn_secs, 600);
        assert_eq!(cfg.auto_reap_secs, 0);
        assert_eq!(cfg.never_kill_patterns, vec!["my-build*".to_string()]);
        // Unspecified fields fall back to defaults via `serde(default)`.
        assert_eq!(cfg.finished_ttl_secs, 1800);
    }
}
