use serde::{Deserialize, Serialize};

/// Self-update prompt configuration.
///
/// `skipped_version` holds the latest GitHub release tag the user dismissed
/// via "Skip until next version" on the TUI-open update prompt. The prompt
/// suppresses itself when the live latest tag equals this value, but
/// re-appears once a newer release lands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Tag the user chose to skip (e.g. "0.3.0"). Empty = no dismissal.
    pub skipped_version: String,
}
