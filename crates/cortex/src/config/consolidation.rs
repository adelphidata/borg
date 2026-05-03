use serde::{Deserialize, Serialize};

/// Memory-consolidation runtime state.
///
/// `last_nightly` and `last_weekly` hold the unix-epoch second at which the
/// corresponding consolidation pass last completed successfully. They are
/// written by `consolidation::run_dream` / `run_single_pass` straight to the
/// `settings` table (`db.set_setting`) and read back through the typed
/// `Config` on the next reload — the registry entries in `settings_table.rs`
/// route them here so `apply_setting` no longer logs "Unknown setting".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// Unix-epoch seconds of the last successful nightly consolidation run.
    pub last_nightly: Option<i64>,
    /// Unix-epoch seconds of the last successful weekly consolidation run.
    pub last_weekly: Option<i64>,
}
