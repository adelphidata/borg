use serde::{Deserialize, Serialize};

/// Daily self-healing maintenance task configuration.
///
/// The daily `Maintenance` scheduled task runs a headless doctor sweep,
/// prunes old logs and activity-log rows, and surfaces persistent warnings
/// to the configured heartbeat channel. These settings tune retention.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceConfig {
    /// Master switch. When false, the seeded task is skipped entirely.
    pub enabled: bool,
    /// Delete `~/.borg/logs/*.jsonl` files older than this many days.
    pub logs_retention_days: u32,
    /// Delete activity-log rows older than this many days.
    pub activity_retention_days: u32,
    /// Keep at most this many rows in the `doctor_runs` history table.
    pub doctor_runs_keep: u32,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            logs_retention_days: 30,
            activity_retention_days: 30,
            doctor_runs_keep: 30,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_defaults_are_sensible() {
        let cfg = MaintenanceConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.logs_retention_days, 30);
        assert_eq!(cfg.activity_retention_days, 30);
        assert_eq!(cfg.doctor_runs_keep, 30);
    }

    #[test]
    fn maintenance_config_from_toml() {
        let toml_str = r#"
            enabled = false
            logs_retention_days = 7
            activity_retention_days = 14
            doctor_runs_keep = 90
        "#;
        let cfg: MaintenanceConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.logs_retention_days, 7);
        assert_eq!(cfg.activity_retention_days, 14);
        assert_eq!(cfg.doctor_runs_keep, 90);
    }
}
