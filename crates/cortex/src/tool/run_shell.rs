//! `run_shell` tool.

use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::policy::ExecutionPolicy;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunShellArgs {
    pub command: String,
    #[serde(default)]
    pub tty: Option<bool>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl RunShellArgs {
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("RunShellArgs is JSON-safe")
    }
}

pub(crate) async fn handle(
    args: &RunShellArgs,
    config: &Config,
    policy: &ExecutionPolicy,
    event_tx: &mpsc::Sender<AgentEvent>,
    skill_env_allowlist: Option<&HashSet<String>>,
) -> Result<String> {
    tool_handlers::handle_run_shell(&args.to_value(), config, policy, event_tx, skill_env_allowlist)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: RunShellArgs =
            serde_json::from_value(serde_json::json!({"command":"echo hi"})).unwrap();
        assert_eq!(args.command, "echo hi");
        assert!(args.tty.is_none());
    }

    #[test]
    fn deserialize_with_tty_and_workdir() {
        let args: RunShellArgs = serde_json::from_value(
            serde_json::json!({"command":"ls","tty":true,"workdir":"/tmp","timeout_ms":1000}),
        )
        .unwrap();
        assert_eq!(args.tty, Some(true));
        assert_eq!(args.workdir.as_deref(), Some("/tmp"));
        assert_eq!(args.timeout_ms, Some(1000));
    }
}
