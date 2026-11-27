use serde::{Deserialize, Serialize};

/// Undo points for file changes.
///
/// Before the first tool call of a turn that can change files, Borg records
/// the workspace in its own store under the data dir (never in the project's
/// `.git`), so the files can be put back afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointsConfig {
    /// Off: no undo points are taken, and file changes cannot be rolled back.
    pub enabled: bool,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
