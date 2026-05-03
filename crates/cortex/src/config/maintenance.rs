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
    /// Whether the maintenance sweep should repair missing/stale memory
    /// embeddings and chunk indexes.
    pub embedding_repair_enabled: bool,
    /// Maximum number of memory entries to repair in one sweep.
    pub embedding_repair_max_files: u32,
    /// Maximum number of chunks to repair in one sweep.
    pub embedding_repair_max_chunks: u32,
    /// Drop resolved issue-state rows older than this many days.
    pub issue_state_keep_days: u32,
    /// How long a `/doctor` squelch persists before the daily sweep prunes
    /// it and the warning resumes. Bounded on purpose — security findings
    /// the user has acknowledged should re-surface periodically rather
    /// than disappear forever.
    pub security_squelch_ttl_days: u32,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            logs_retention_days: 30,
            activity_retention_days: 30,
            doctor_runs_keep: 30,
            embedding_repair_enabled: true,
            embedding_repair_max_files: 16,
            embedding_repair_max_chunks: 256,
            issue_state_keep_days: 30,
            security_squelch_ttl_days: 30,
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
        assert!(cfg.embedding_repair_enabled);
        assert_eq!(cfg.embedding_repair_max_files, 16);
        assert_eq!(cfg.embedding_repair_max_chunks, 256);
        assert_eq!(cfg.issue_state_keep_days, 30);
        assert_eq!(cfg.security_squelch_ttl_days, 30);
    }

    #[test]
    fn maintenance_config_from_toml() {
        let toml_str = r#"
            enabled = false
            logs_retention_days = 7
            activity_retention_days = 14
            doctor_runs_keep = 90
            embedding_repair_enabled = false
            embedding_repair_max_files = 4
            embedding_repair_max_chunks = 64
            issue_state_keep_days = 10
        "#;
        let cfg: MaintenanceConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.logs_retention_days, 7);
        assert_eq!(cfg.activity_retention_days, 14);
        assert_eq!(cfg.doctor_runs_keep, 90);
        assert!(!cfg.embedding_repair_enabled);
        assert_eq!(cfg.embedding_repair_max_files, 4);
        assert_eq!(cfg.embedding_repair_max_chunks, 64);
        assert_eq!(cfg.issue_state_keep_days, 10);
    }
}
