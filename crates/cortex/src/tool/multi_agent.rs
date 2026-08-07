//! Multi-agent tools: `spawn_agent`, `send_to_agent`, `wait_for_agent`,
//! `list_agents`, `close_agent`, `manage_roles`.
//!
//! These tools all share a permissive arg shape — each underlying handler
//! validates per-action — so a single `MultiAgentArgs` newtype carries the raw
//! payload through, mirroring the `ScheduleArgs` and `BrowserArgs` pattern.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::multi_agent::{self, AgentControl};
use crate::types::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct MultiAgentArgs(pub serde_json::Value);

impl MultiAgentArgs {
    pub(crate) fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

const MULTI_AGENT_DISABLED: &str = "Error: Multi-agent system is not enabled.";

pub(crate) async fn handle_spawn_agent(
    args: &MultiAgentArgs,
    agent_control: &mut Option<AgentControl>,
    config: &Config,
    history: &[Message],
) -> Result<String> {
    if let Some(ref mut ctrl) = agent_control {
        let hist = if args.as_value()["fork_context"].as_bool().unwrap_or(false) {
            Some(history)
        } else {
            None
        };
        multi_agent::tools::handle_spawn_agent(args.as_value(), ctrl, config, hist).await
    } else {
        Ok(MULTI_AGENT_DISABLED.to_string())
    }
}

pub(crate) async fn handle_send_to_agent(
    args: &MultiAgentArgs,
    agent_control: &Option<AgentControl>,
) -> Result<String> {
    if let Some(ctrl) = agent_control {
        multi_agent::tools::handle_send_to_agent(args.as_value(), ctrl).await
    } else {
        Ok(MULTI_AGENT_DISABLED.to_string())
    }
}

pub(crate) async fn handle_wait_for_agent(
    args: &MultiAgentArgs,
    agent_control: &mut Option<AgentControl>,
) -> Result<String> {
    if let Some(ref mut ctrl) = agent_control {
        multi_agent::tools::handle_wait_for_agent(args.as_value(), ctrl).await
    } else {
        Ok(MULTI_AGENT_DISABLED.to_string())
    }
}

pub(crate) fn handle_list_agents(agent_control: Option<&AgentControl>) -> Result<String> {
    if let Some(ctrl) = agent_control {
        multi_agent::tools::handle_list_agents(ctrl)
    } else {
        Ok(MULTI_AGENT_DISABLED.to_string())
    }
}

pub(crate) async fn handle_close_agent(
    args: &MultiAgentArgs,
    agent_control: &mut Option<AgentControl>,
) -> Result<String> {
    if let Some(ref mut ctrl) = agent_control {
        multi_agent::tools::handle_close_agent(args.as_value(), ctrl).await
    } else {
        Ok(MULTI_AGENT_DISABLED.to_string())
    }
}

pub(crate) async fn handle_manage_roles(args: &MultiAgentArgs) -> Result<String> {
    multi_agent::tools::handle_manage_roles(args.as_value()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_agent_without_control_reports_disabled() {
        let mut ctrl: Option<AgentControl> = None;
        let config = Config::default();
        let args = MultiAgentArgs(serde_json::json!({}));
        let out = handle_spawn_agent(&args, &mut ctrl, &config, &[])
            .await
            .expect("returns Ok");
        assert!(out.contains("not enabled"), "got: {out}");
    }

    /// REGRESSION: this dispatch used to return a hardcoded "unavailable in
    /// this build" string, so the real `send_input` path — steer channel,
    /// pending-input ledger, undelivered reporting — was unreachable from the
    /// agent loop. With multi-agent off it must still degrade to the same
    /// structured string the other multi-agent tools use: returning `Err`
    /// would abort the turn instead of letting the model pick another tool.
    #[tokio::test]
    async fn send_to_agent_without_multi_agent_reports_disabled_not_unsupported() {
        let ctrl: Option<AgentControl> = None;
        let args = MultiAgentArgs(serde_json::json!({
            "agent_id": "agent-x",
            "message": "change of plan",
        }));
        let out = handle_send_to_agent(&args, &ctrl)
            .await
            .expect("returns Ok");
        assert_eq!(out, MULTI_AGENT_DISABLED, "got: {out}");
    }
}
