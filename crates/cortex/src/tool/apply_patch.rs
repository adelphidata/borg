//! `apply_patch`, `apply_skill_patch`, and `create_channel` tools.
//!
//! All three take the same shape: a `patch` body in the patch DSL plus an
//! optional `target` directing where it lands. They share an [`ApplyPatchArgs`].

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApplyPatchArgs {
    pub patch: String,
    /// `cwd`, `skills`, or `channels`. Only honoured by the `apply_patch`
    /// variant; aliased tools (`apply_skill_patch`, `create_channel`) ignore it.
    #[serde(default)]
    pub target: Option<String>,
}

impl ApplyPatchArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of strings + Option<String>; serialization is infallible.
        // The expect would only fire on a serde upstream bug, not runtime data.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("ApplyPatchArgs is JSON-safe")
        }
    }
}

/// Unified `apply_patch` (`target` selects cwd/skills/channels).
pub(crate) fn handle_apply_patch(args: &ApplyPatchArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_apply_patch_unified(&args.to_value(), config)
}

/// `apply_skill_patch` alias — always lands in `~/.borg/skills/`.
pub(crate) fn handle_apply_skill_patch(args: &ApplyPatchArgs) -> Result<String> {
    tool_handlers::handle_apply_skill_patch(&args.to_value())
}

/// `create_channel` alias — always lands in `~/.borg/channels/`.
pub(crate) fn handle_create_channel(args: &ApplyPatchArgs) -> Result<String> {
    tool_handlers::handle_create_channel(&args.to_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: ApplyPatchArgs =
            serde_json::from_value(serde_json::json!({"patch":"*** Begin Patch\n*** End Patch"}))
                .unwrap();
        assert!(args.patch.starts_with("*** Begin"));
        assert!(args.target.is_none());
    }

    #[test]
    fn deserialize_with_target() {
        let args: ApplyPatchArgs =
            serde_json::from_value(serde_json::json!({"patch":"x","target":"skills"})).unwrap();
        assert_eq!(args.target.as_deref(), Some("skills"));
    }
}
